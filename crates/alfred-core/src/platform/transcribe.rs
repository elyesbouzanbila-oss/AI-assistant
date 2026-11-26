//! Speech-to-text for WAV recordings.
//!
//! Windows recognises audio with the `System.Speech` assembly that ships with
//! the OS, driven from the same hidden PowerShell used for window control and
//! audio capture. No audio crate, no WinRT package identity, no network. The
//! transcript is plain text, so the agent loop never sees audio: whichever
//! provider produced it, the text model answers the turn afterwards.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use tokio::process::Command;

/// Matches the recorder's own deadline, so an abandoned recognition cannot
/// keep PowerShell alive after the recording is gone.
const RECOGNISE_TIMEOUT: Duration = Duration::from_secs(40);

/// Several utterances are collected rather than stopping at the first pause:
/// a dictation can be many sentences long. Events are used because `Recognize`
/// on a wave file returns after a single utterance. Failures are written to
/// stdout - stderr is piped but never read, so a bare `throw` would surface as
/// the useless "(no response)".
const TRANSCRIBE_SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
try {
  Add-Type -AssemblyName System.Speech
  $path = $env:ALFRED_ARG
  if ([string]::IsNullOrWhiteSpace($path)) { throw 'no recording path given' }

  $global:AlfredParts = New-Object System.Collections.Generic.List[string]
  $global:AlfredDone = $false
  $recognizer = New-Object System.Speech.Recognition.SpeechRecognitionEngine
  $recognizer.LoadGrammar((New-Object System.Speech.Recognition.DictationGrammar))
  $recognizer.SetInputToWaveFile($path)

  $onRecognized = Register-ObjectEvent -InputObject $recognizer -EventName SpeechRecognized -Action {
    try {
      if ($null -ne $EventArgs.Result -and $EventArgs.Result.Text) {
        $global:AlfredParts.Add([string]$EventArgs.Result.Text)
      }
    } catch { }
  }
  $onCompleted = Register-ObjectEvent -InputObject $recognizer -EventName RecognizeCompleted -Action {
    $global:AlfredDone = $true
  }
  $recognizer.RecognizeAsync([System.Speech.Recognition.RecognizeMode]::Multiple)

  $deadline = [DateTime]::UtcNow.AddSeconds(35)
  while (-not $global:AlfredDone -and [DateTime]::UtcNow -lt $deadline) {
    Start-Sleep -Milliseconds 50
  }
  # Read the result before cleanup: an event job that already went away must
  # not turn a recognised sentence into an error.
  $text = ($global:AlfredParts -join ' ')
  try { Unregister-Event -SubscriptionId $onRecognized.Id } catch { }
  try { Unregister-Event -SubscriptionId $onCompleted.Id } catch { }
  try { $recognizer.RecognizeAsyncCancel() } catch { }
  $recognizer.Dispose()

  $text
} catch {
  $_.Exception.Message
  exit 1
}
"#;

/// Recognise a WAV recording on this machine and return its transcript.
pub async fn transcribe_wav(wav: &[u8]) -> Result<String> {
    if !cfg!(windows) {
        bail!("Windows speech recognition is not available on this platform");
    }

    let path = temp_wav_path();
    tokio::fs::write(&path, wav)
        .await
        .with_context(|| format!("could not stage audio at {}", path.display()))?;

    let mut command = Command::new("powershell.exe");
    command
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            TRANSCRIBE_SCRIPT,
        ])
        .env("ALFRED_ARG", path.to_string_lossy().to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    let output = tokio::time::timeout(RECOGNISE_TIMEOUT, command.output()).await;

    // A recording never outlives the request, including after a timeout.
    let _ = tokio::fs::remove_file(&path).await;

    let output = output
        .map_err(|_| anyhow!("speech recognition did not finish in time"))?
        .context("could not start powershell.exe for transcription")?;

    let text = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() {
        bail!(
            "Windows speech recognition failed: {}",
            text.trim().chars().take(300).collect::<String>()
        );
    }
    transcript(&text)
}

/// Run whisper.cpp inside WSL and read back the text file it writes. The
/// binary and model are Linux paths; the staged WAV crosses through `/mnt/c`.
/// `wsl.exe -e` passes arguments directly, so nothing reaches a shell.
pub async fn transcribe_with_whisper(wav: &[u8], binary: &str, model: &str) -> Result<String> {
    if !cfg!(windows) {
        bail!("Whisper transcription requires wsl.exe on Windows");
    }

    let wav_path = temp_wav_path();
    let txt_path = wav_path.with_extension("txt");
    let base = wav_path.with_extension("");
    tokio::fs::write(&wav_path, wav)
        .await
        .with_context(|| format!("could not stage audio at {}", wav_path.display()))?;

    let wsl_wav = to_wsl_path(&wav_path);
    let wsl_base = to_wsl_path(&base);
    let mut command = Command::new("wsl.exe");
    command
        .args([
            "-e", binary, "-m", model, "-f", wsl_wav.as_str(), "-ngl", "0", "-t", "4", "-otxt",
            "-of", wsl_base.as_str(),
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    let output = tokio::time::timeout(Duration::from_secs(180), command.output()).await;

    // Neither the staged audio nor the transcript outlives the request.
    let text = tokio::fs::read_to_string(&txt_path).await.unwrap_or_default();
    let _ = tokio::fs::remove_file(&txt_path).await;
    let _ = tokio::fs::remove_file(&wav_path).await;

    let output = output
        .map_err(|_| anyhow!("whisper.cpp did not finish in time"))?
        .context("could not start wsl.exe for transcription")?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr)
            .trim()
            .chars()
            .take(300)
            .collect::<String>();
        bail!("whisper.cpp failed: {}", detail);
    }
    transcript(&text)
}

/// Windows temp paths are visible inside WSL under `/mnt/<drive>`.
fn to_wsl_path(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    let chars: Vec<char> = text.chars().collect();
    if chars.len() > 2 && chars[1] == ':' {
        format!("/mnt/{}/{}", chars[0].to_ascii_lowercase(), &text[3..])
    } else {
        text
    }
}

fn temp_wav_path() -> PathBuf {
    std::env::temp_dir().join(format!(
        "alfred-stt-{}-{}.wav",
        std::process::id(),
        chrono::Local::now().timestamp_millis()
    ))
}

/// Same normalisation as the model transcriber: quotes are an artefact of the
/// answer, not part of what was said.
fn transcript(text: &str) -> Result<String> {
    let clean = text.trim().trim_matches('"').trim().to_string();
    if clean.is_empty() {
        bail!("no speech was recognised in that recording");
    }
    Ok(clean)
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reports_a_clear_error_off_windows() {
        let error = transcribe_wav(b"not a wav").await.unwrap_err();
        assert!(error.to_string().contains("Windows"), "{error}");
    }

    #[test]
    fn windows_paths_become_wsl_paths() {
        assert_eq!(
            to_wsl_path(Path::new(r"C:\Users\Chief\AppData\Local\Temp\a.wav")),
            "/mnt/c/Users/Chief/AppData/Local/Temp/a.wav"
        );
        assert_eq!(
            to_wsl_path(Path::new("/home/Chief/models/ggml-small.en.bin")),
            "/home/Chief/models/ggml-small.en.bin"
        );
    }

    #[test]
    fn strips_quotes_and_rejects_silence() {
        assert_eq!(transcript("  \"open notepad\"  ").unwrap(), "open notepad");
        assert!(transcript("  \"\"  ").is_err());
        assert!(transcript("   ").is_err());
    }
}
