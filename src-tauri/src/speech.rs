//! Speak the answer back, through the voice Windows already ships.
//!
//! SAPI runs in its own hidden PowerShell and blocks inside `Speak`, so stopping
//! is killing that process: there is no in-band cancel. The text and the voice
//! name travel in environment variables and the script arrives on stdin, so an
//! answer written by a model can never become PowerShell source - the same rule
//! as `ALFRED_ARG`.

use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;

/// One utterance at a time: a new answer stops the previous one.
static CURRENT: Mutex<Option<Child>> = Mutex::new(None);

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Fed to `powershell -Command -` on stdin, so nothing needs quoting.
const SCRIPT: &str = r#"
Add-Type -AssemblyName System.Speech
$synth = New-Object System.Speech.Synthesis.SpeechSynthesizer
if ($env:ALFRED_SPEECH_VOICE) {
  try { $synth.SelectVoice($env:ALFRED_SPEECH_VOICE) } catch { }
}
$synth.Rate = [int]$env:ALFRED_SPEECH_RATE
$synth.Speak([string]$env:ALFRED_SPEECH_TEXT)
"#;

/// Start speaking. Returns once the process exists, not when it finishes.
#[cfg(windows)]
pub fn speak(text: &str, voice: &str, rate: i32) -> Result<(), String> {
    use std::os::windows::process::CommandExt;

    stop();

    let mut child = Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            "-",
        ])
        .env("ALFRED_SPEECH_TEXT", text)
        .env("ALFRED_SPEECH_VOICE", voice)
        .env("ALFRED_SPEECH_RATE", clamp_rate(rate).to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|error| format!("could not start the speech process: {error}"))?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(SCRIPT.as_bytes())
            .map_err(|error| format!("could not hand the script to PowerShell: {error}"))?;
    }

    // A voice that fails mid-sentence is silence, not a turn error: the status
    // is not worth a reaper thread, and speech is never retried.
    let mut slot = CURRENT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *slot = Some(child);
    Ok(())
}

/// There is no voice without Windows; the rest of the app does not care.
#[cfg(not(windows))]
pub fn speak(_text: &str, _voice: &str, _rate: i32) -> Result<(), String> {
    Ok(())
}

/// Kill the current utterance. Idempotent: nothing to stop is success.
pub fn stop() {
    let mut slot = CURRENT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(mut child) = slot.take() {
        let _ = child.kill();
        let _ = child.wait();
    }
}

#[cfg(windows)]
fn clamp_rate(rate: i32) -> i32 {
    rate.clamp(-10, 10)
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn rate_is_clamped_to_what_sapi_accepts() {
        assert_eq!(clamp_rate(0), 0);
        assert_eq!(clamp_rate(-42), -10);
        assert_eq!(clamp_rate(99), 10);
    }
}
