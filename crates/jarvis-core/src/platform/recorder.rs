//! Microphone capture.
//!
//! Windows records with MCI (`waveaudio`), which ships with the OS: no audio
//! crate, no WASAPI bindings, and the same "reproduce it by hand in a terminal"
//! property as the rest of the Windows backend. One hidden `powershell.exe`
//! records; writing a stop file stops it and saves a WAV, which the caller
//! transcribes and then deletes.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStdout, Command};

/// Longest single recording; the island stops at the same point.
pub const MAX_SECONDS: u64 = 30;

/// `JARVIS_ARG` carries the output path, so nothing is interpolated into
/// PowerShell source. Failures are printed on stdout: a thrown error only
/// reaches stderr, which the caller does not read.
const RECORD_SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
try {
  Add-Type -Namespace JarvisAudio -Name Mci -MemberDefinition '[DllImport("winmm.dll", CharSet=CharSet.Auto)] public static extern int mciSendString(string command, System.Text.StringBuilder buffer, int bufferSize, IntPtr hwnd); [DllImport("winmm.dll", CharSet=CharSet.Auto)] public static extern int mciGetErrorString(int code, System.Text.StringBuilder buffer, int size);'
  function Send-Mci([string]$command) {
    $buffer = New-Object System.Text.StringBuilder 256
    return [JarvisAudio.Mci]::mciSendString($command, $buffer, 256, [IntPtr]::Zero)
  }
  function Get-MciError([int]$code) {
    $buffer = New-Object System.Text.StringBuilder 256
    [void][JarvisAudio.Mci]::mciGetErrorString($code, $buffer, 256)
    return $buffer.ToString().Trim()
  }

  $path = $env:JARVIS_ARG
  if ([string]::IsNullOrWhiteSpace($path)) { throw 'no recording path given' }

  # Devices disagree about what they will record, and MCI accepts the `set`
  # commands before refusing the record itself, so try the small formats first
  # and finish on whatever the device records by default. The WAV header says
  # which one was used, so the upload stays valid either way.
  $code = 0
  foreach ($rate in @(16000, 22050, 32000, 0)) {
    $opened = Send-Mci 'open new type waveaudio alias rec'
    if ($opened -ne 0) { throw ('could not open the microphone: ' + (Get-MciError $opened)) }
    if ($rate -gt 0) {
      [void](Send-Mci 'set rec time format milliseconds')
      [void](Send-Mci 'set rec channels 1')
      [void](Send-Mci ('set rec samplespersec ' + $rate))
      [void](Send-Mci 'set rec bitspersample 16')
    }
    $code = Send-Mci 'record rec'
    if ($code -eq 0) { break }
    [void](Send-Mci 'close rec')
  }
  if ($code -ne 0) { throw ('could not start recording: ' + (Get-MciError $code)) }

  'recording'
  # stdin is not a reliable signal: with no console attached PowerShell's
  # ReadLine hits end-of-stream at once, which cut recordings to a fraction of
  # a second. Poll for a file instead, and give up on our own after the island's
  # limit so an abandoned recording cannot run forever.
  $stopFile = $path + '.stop'
  $deadline = [DateTime]::UtcNow.AddSeconds(35)
  while (-not (Test-Path -LiteralPath $stopFile) -and [DateTime]::UtcNow -lt $deadline) {
    Start-Sleep -Milliseconds 50
  }
  [void](Send-Mci 'stop rec')
  $saved = Send-Mci ('save rec "' + $path + '"')
  if ($saved -ne 0) { [void](Send-Mci 'close rec'); throw ('could not save the recording: ' + (Get-MciError $saved)) }
  [void](Send-Mci 'close rec')
  'saved'
} catch {
  $_.Exception.Message
  exit 1
}
"#;

pub struct Recorder {
    child: Child,
    /// Held so the pipe stays open while recording; nothing is read from it.
    _stdout: ChildStdout,
    path: PathBuf,
    /// Created by [`Recorder::stop`]; the script polls for it. Its stdin cannot
    /// carry the signal because a console-less PowerShell reports end-of-stream
    /// immediately, which ended recordings almost as soon as they started.
    stop_path: PathBuf,
}

impl Recorder {
    /// Start recording; returns once the microphone is actually open, so a
    /// missing or blocked device fails immediately instead of silently.
    pub async fn start() -> Result<Self> {
        if !cfg!(windows) {
            bail!(
                "voice input uses the Windows microphone API and is not available on this platform"
            );
        }

        let path = std::env::temp_dir().join(format!(
            "jarvis-voice-{}-{}.wav",
            std::process::id(),
            chrono::Local::now().timestamp_millis()
        ));
        // Must match `$path + '.stop'` in the script.
        let stop_path = path.with_extension("wav.stop");

        let mut command = Command::new("powershell.exe");
        command
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                RECORD_SCRIPT,
            ])
            .env("JARVIS_ARG", path.to_string_lossy().to_string())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        #[cfg(windows)]
        {
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }

        let mut child = command
            .spawn()
            .context("could not start powershell.exe for recording")?;
        let stdout = child
            .stdout
            .take()
            .context("recording process has no output")?;

        let mut lines = BufReader::new(stdout).lines();
        let waited = tokio::time::timeout(Duration::from_secs(10), lines.next_line()).await;
        let ready = matches!(&waited, Ok(Ok(Some(line))) if line.trim() == "recording");

        if !ready {
            let _ = child.start_kill();
            let detail = match waited {
                Ok(Ok(Some(line))) => line.trim().chars().take(300).collect(),
                Ok(_) => "no response".to_string(),
                Err(_) => "timed out".to_string(),
            };
            bail!(
                "the microphone did not start ({detail}). Is one connected and allowed for desktop apps?"
            );
        }

        Ok(Self {
            child,
            _stdout: lines.into_inner().into_inner(),
            path,
            stop_path,
        })
    }

    /// Stop, wait for the WAV, and return its bytes. The file is always
    /// removed, so a recording never outlives the request.
    pub async fn stop(mut self) -> Result<Vec<u8>> {
        tokio::fs::write(&self.stop_path, b"")
            .await
            .with_context(|| {
                format!(
                    "could not signal the recorder at {}",
                    self.stop_path.display()
                )
            })?;

        let stopped = match tokio::time::timeout(Duration::from_secs(20), self.child.wait()).await {
            Ok(Ok(status)) if status.success() => Ok(()),
            Ok(_) => Err(anyhow::anyhow!(
                "the recorder stopped before saving the audio"
            )),
            Err(_) => {
                let _ = self.child.start_kill();
                Err(anyhow::anyhow!("the recorder did not stop in time"))
            }
        };

        let bytes = tokio::fs::read(&self.path)
            .await
            .with_context(|| format!("no recording at {}", self.path.display()));
        let _ = tokio::fs::remove_file(&self.path).await;
        let _ = tokio::fs::remove_file(&self.stop_path).await;

        stopped?;
        bytes
    }
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reports_a_clear_error_off_windows() {
        let message = match Recorder::start().await {
            Ok(_) => panic!("recording must not start off Windows"),
            Err(error) => error.to_string(),
        };
        assert!(message.contains("Windows"), "{message}");
    }
}
