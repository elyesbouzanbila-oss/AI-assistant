//! Microphone capture.
//!
//! Windows records with MCI (`waveaudio`), which ships with the OS: no audio
//! crate, no WASAPI bindings, and the same "reproduce it by hand in a terminal"
//! property as the rest of the Windows backend. One hidden `powershell.exe`
//! records; writing a line to its stdin stops it and saves a WAV, which the
//! caller transcribes and then deletes.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

/// Longest single recording; the island stops at the same point.
pub const MAX_SECONDS: u64 = 30;

/// `JARVIS_ARG` carries the output path, so nothing is interpolated into
/// PowerShell source.
const RECORD_SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
Add-Type -Namespace JarvisAudio -Name Mci -MemberDefinition '[DllImport("winmm.dll", CharSet=CharSet.Auto)] public static extern int mciSendString(string command, System.Text.StringBuilder buffer, int bufferSize, IntPtr hwnd);'
function Send-Mci([string]$command) {
  $buffer = New-Object System.Text.StringBuilder 256
  return [JarvisAudio.Mci]::mciSendString($command, $buffer, 256, [IntPtr]::Zero)
}
$path = $env:JARVIS_ARG
if ([string]::IsNullOrWhiteSpace($path)) { throw 'no recording path given' }
if ((Send-Mci 'open new type waveaudio alias rec') -ne 0) { throw 'could not open the microphone' }
# Best effort: 16 kHz mono keeps the upload small. If the device refuses, it
# records in its default format and the WAV header still says what that is.
[void](Send-Mci 'set rec time format milliseconds')
[void](Send-Mci 'set rec channels 1')
[void](Send-Mci 'set rec samplespersec 16000')
[void](Send-Mci 'set rec bitspersample 16')
if ((Send-Mci 'record rec') -ne 0) { [void](Send-Mci 'close rec'); throw 'could not start recording' }
'recording'
[Console]::In.ReadLine() | Out-Null
[void](Send-Mci 'stop rec')
if ((Send-Mci ('save rec "' + $path + '"')) -ne 0) { [void](Send-Mci 'close rec'); throw 'could not save the recording' }
[void](Send-Mci 'close rec')
'saved'
"#;

pub struct Recorder {
    child: Child,
    stdin: Option<ChildStdin>,
    /// Held so the pipe stays open while recording; nothing is read from it.
    _stdout: ChildStdout,
    path: PathBuf,
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
            .stdin(std::process::Stdio::piped())
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
        let stdin = child.stdin.take();
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
                Ok(Ok(Some(line))) => line.trim().to_string(),
                Ok(_) => "no response".to_string(),
                Err(_) => "timed out".to_string(),
            };
            bail!(
                "the microphone did not start ({detail}). Is one connected and allowed for desktop apps?"
            );
        }

        Ok(Self {
            child,
            stdin,
            _stdout: lines.into_inner().into_inner(),
            path,
        })
    }

    /// Stop, wait for the WAV, and return its bytes. The file is always
    /// removed, so a recording never outlives the request.
    pub async fn stop(mut self) -> Result<Vec<u8>> {
        let mut stdin_dropped = self.stdin.take();
        if let Some(stdin) = stdin_dropped.as_mut() {
            let _ = stdin.write_all(b"\n").await;
            let _ = stdin.flush().await;
        }
        drop(stdin_dropped);

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
