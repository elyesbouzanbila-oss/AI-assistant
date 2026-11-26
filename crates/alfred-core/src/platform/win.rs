//! Windows backend.
//!
//! App and window control goes through PowerShell: no FFI bindings to keep in
//! sync with a `windows` crate version, and every action is something you can
//! reproduce by hand in a terminal. Model-supplied text is passed via the
//! `ALFRED_ARG` environment variable, never interpolated into the script, so a
//! prompt injection cannot turn a tool call into arbitrary PowerShell.
//!
//! Input injection uses enigo (`SendInput` under the hood).

use anyhow::{bail, Context, Result};
use std::process::Stdio;

use super::{input, screenshot, Platform, PlatformFuture};

pub struct WindowsPlatform;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

async fn powershell(script: &str, arg: Option<&str>) -> Result<String> {
    let mut command = tokio::process::Command::new("powershell.exe");
    command
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            script,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if let Some(arg) = arg {
        command.env("ALFRED_ARG", arg);
    }

    // Keep the console from flashing up on every action.
    #[cfg(windows)]
    {
        command.creation_flags(CREATE_NO_WINDOW);
    }

    let output = command
        .output()
        .await
        .context("could not start powershell.exe")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let message: String = stderr.trim().chars().take(300).collect();
        bail!("powershell failed: {message}");
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// List the Start-menu app that best matches `name`.
const OPEN_APP: &str = r#"
$ErrorActionPreference = 'Stop'
$n = $env:ALFRED_ARG
if ([string]::IsNullOrWhiteSpace($n)) { throw 'no application name given' }
$app = Get-StartApps | Where-Object { $_.Name -like "*$n*" } | Select-Object -First 1
if ($null -ne $app) { Start-Process ("shell:AppsFolder\" + $app.AppID); 'opened ' + $app.Name; return }
try { Start-Process $n; 'started ' + $n } catch { throw ("no application matching '" + $n + "'") }
"#;

/// Ask a window to close. Deliberately `CloseMainWindow` only: killing a
/// process is how you lose unsaved work, so this never escalates.
const CLOSE_APP: &str = r#"
$ErrorActionPreference = 'Stop'
$n = $env:ALFRED_ARG
$procs = @(Get-Process | Where-Object { ($_.MainWindowTitle -like "*$n*") -or ($_.ProcessName -like "*$n*") })
if ($procs.Count -eq 0) { "nothing matched '$n'"; return }
$closed = @()
foreach ($p in $procs) { if ($p.CloseMainWindow()) { $closed += $p.ProcessName } }
if ($closed.Count -eq 0) { "matched $($procs.Count) process(es) but none had a window to close" }
else { 'asked to close: ' + ($closed -join ', ') }
"#;

const FOCUS_WINDOW: &str = r#"
$ErrorActionPreference = 'Stop'
$n = $env:ALFRED_ARG
Add-Type -Namespace AlfredNative -Name Win -MemberDefinition '[DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd); [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hWnd, int nCmdShow);'
$p = Get-Process | Where-Object { $_.MainWindowTitle -like "*$n*" } | Select-Object -First 1
if ($null -eq $p) { "no window matching '$n'"; return }
[AlfredNative.Win]::ShowWindow($p.MainWindowHandle, 9) | Out-Null
[AlfredNative.Win]::SetForegroundWindow($p.MainWindowHandle) | Out-Null
'focused ' + $p.MainWindowTitle
"#;

const LIST_WINDOWS: &str = r#"
$items = @(Get-Process | Where-Object { $_.MainWindowTitle -ne '' } | Select-Object -First 40 |
  ForEach-Object { $_.ProcessName + ' | ' + $_.MainWindowTitle })
if ($items.Count -eq 0) { 'no visible windows' } else { $items -join "`n" }
"#;

/// Text currently on the clipboard, verbatim.
const GET_CLIPBOARD: &str = r#"
$ErrorActionPreference = 'Stop'
Get-Clipboard -Raw
"#;

/// The text arrives in `ALFRED_ARG`, never in the script source: copying a
/// pasted URL must not be able to become a command.
const SET_CLIPBOARD: &str = r#"
$ErrorActionPreference = 'Stop'
Set-Clipboard -Value $env:ALFRED_ARG
'ok'
"#;

/// Save the whole virtual desktop to the path in `ALFRED_ARG`.
const SCREENSHOT: &str = r#"
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
$vs = [System.Windows.Forms.SystemInformation]::VirtualScreen
$bmp = New-Object System.Drawing.Bitmap $vs.Width, $vs.Height
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.CopyFromScreen($vs.X, $vs.Y, 0, 0, $bmp.Size)
$bmp.Save($env:ALFRED_ARG, [System.Drawing.Imaging.ImageFormat]::Png)
$g.Dispose(); $bmp.Dispose()
'saved'
"#;

/// Run one blocking input call off the async runtime. These actions have
/// nothing to report, so they return an empty result string.
fn on_input_thread<F>(work: F) -> PlatformFuture<'static>
where
    F: FnOnce() -> Result<()> + Send + 'static,
{
    Box::pin(async move {
        tokio::task::spawn_blocking(work)
            .await
            .context("input thread panicked")??;
        Ok(String::new())
    })
}

impl Platform for WindowsPlatform {
    fn name(&self) -> &'static str {
        "Windows"
    }

    fn open_app<'a>(&'a self, name: &'a str) -> PlatformFuture<'a> {
        Box::pin(async move { powershell(OPEN_APP, Some(name)).await })
    }

    fn close_app<'a>(&'a self, target: &'a str) -> PlatformFuture<'a> {
        Box::pin(async move { powershell(CLOSE_APP, Some(target)).await })
    }

    fn focus_window<'a>(&'a self, target: &'a str) -> PlatformFuture<'a> {
        Box::pin(async move { powershell(FOCUS_WINDOW, Some(target)).await })
    }

    fn list_windows(&self) -> PlatformFuture<'_> {
        Box::pin(async move { powershell(LIST_WINDOWS, None).await })
    }

    fn type_text<'a>(&'a self, text: &'a str) -> PlatformFuture<'a> {
        let text = text.to_string();
        on_input_thread(move || input::type_text(&text))
    }

    fn press_keys<'a>(&'a self, combo: &'a str) -> PlatformFuture<'a> {
        let combo = combo.to_string();
        on_input_thread(move || input::press_keys(&combo))
    }

    fn click(&self, x: i32, y: i32) -> PlatformFuture<'_> {
        on_input_thread(move || input::click(x, y))
    }

    fn scroll(&self, clicks: i32, position: Option<(i32, i32)>) -> PlatformFuture<'_> {
        on_input_thread(move || input::scroll(clicks, position))
    }

    fn get_clipboard(&self) -> PlatformFuture<'_> {
        Box::pin(async move { powershell(GET_CLIPBOARD, None).await })
    }

    fn set_clipboard<'a>(&'a self, text: &'a str) -> PlatformFuture<'a> {
        Box::pin(async move {
            powershell(SET_CLIPBOARD, Some(text)).await?;
            Ok(String::new())
        })
    }

    fn screenshot(&self) -> PlatformFuture<'_> {
        Box::pin(async move {
            let path = std::env::temp_dir().join(format!(
                "alfred-shot-{}-{}.png",
                std::process::id(),
                chrono::Local::now().timestamp_millis()
            ));
            let path_string = path.to_string_lossy().to_string();

            powershell(SCREENSHOT, Some(&path_string)).await?;

            let bytes = tokio::fs::read(&path).await.with_context(|| {
                format!("screenshot file was not written to {}", path.display())
            })?;
            let _ = tokio::fs::remove_file(&path).await;

            screenshot::to_data_url(&bytes)
        })
    }
}
