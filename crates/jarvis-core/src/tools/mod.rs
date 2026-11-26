//! Tool registry: schemas handed to the model, human summaries for the confirm
//! chip, and dispatch into the platform layer.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::config::resolve_in_workspace;
use crate::platform::Platform;

const MAX_READ_BYTES: u64 = 64 * 1024;
/// Most entries one `list_files` call reports.
const MAX_LIST_ENTRIES: usize = 200;
/// Largest clipboard write. Windows caps a whole process environment block
/// at 32767 characters, and the text travels as `JARVIS_ARG`.
const MAX_CLIPBOARD_BYTES: usize = 8_000;

pub struct ToolContext {
    pub workspace: PathBuf,
    pub platform: Arc<dyn Platform>,
    /// Whether the model can take an image. Off means the screenshot tool is
    /// neither offered nor executed: its result would end the turn.
    pub vision: bool,
}

pub struct ToolOutcome {
    /// Text returned to the model as the tool result.
    pub text: String,
    /// One-liner for the island and the audit log.
    pub summary: String,
    /// Optional screenshot to attach to the next turn.
    pub image: Option<String>,
}

/// Tool definitions in OpenAI `tools` format.
pub fn schema_values(vision: bool) -> Vec<Value> {
    let mut schemas = vec![
        function(
            "open_app",
            "Open an application by name, or open a URL / settings URI. Examples: 'Notepad', 'Spotify', 'https://example.com', 'ms-settings:display'.",
            json!({
                "type": "object",
                "properties": { "name": { "type": "string", "description": "Application name, URL or settings URI" } },
                "required": ["name"]
            }),
        ),
        function(
            "close_app",
            "Ask an application's window to close gracefully. Unsaved work may prompt the user. Never kills processes.",
            json!({
                "type": "object",
                "properties": { "target": { "type": "string", "description": "Process name or part of the window title" } },
                "required": ["target"]
            }),
        ),
        function(
            "focus_window",
            "Bring a window to the foreground so it receives typing.",
            json!({
                "type": "object",
                "properties": { "target": { "type": "string", "description": "Part of the window title" } },
                "required": ["target"]
            }),
        ),
        function(
            "list_windows",
            "List the windows currently open on the desktop.",
            json!({ "type": "object", "properties": {} }),
        ),
        function(
            "type_text",
            "Type literal text into whatever window currently has focus. Use focus_window first if you need a specific app.",
            json!({
                "type": "object",
                "properties": { "text": { "type": "string", "description": "Text to type, exactly as it should appear" } },
                "required": ["text"]
            }),
        ),
        function(
            "press_keys",
            "Press a key combination, e.g. 'ctrl+s', 'alt+f4', 'ctrl+shift+t', 'ctrl+enter'.",
            json!({
                "type": "object",
                "properties": { "combo": { "type": "string", "description": "Key chord, modifiers first" } },
                "required": ["combo"]
            }),
        ),
        function(
            "click",
            "Move the mouse to absolute screen coordinates and left-click. Coordinates are physical pixels; take a screenshot first to find them.",
            json!({
                "type": "object",
                "properties": {
                    "x": { "type": "integer", "description": "X in screen pixels" },
                    "y": { "type": "integer", "description": "Y in screen pixels" }
                },
                "required": ["x", "y"]
            }),
        ),
        function(
            "wait",
            "Wait before the next step: a fixed delay, or until a window whose title contains the given text appears. Prefer waiting for the window over guessing a delay.",
            json!({
                "type": "object",
                "properties": {
                    "ms": { "type": "integer", "description": "Delay in milliseconds, 1 to 10000" },
                    "window": { "type": "string", "description": "Wait until a window title contains this text" },
                    "timeout_ms": { "type": "integer", "description": "How long to wait for the window; default 5000, max 10000" }
                },
            }),
        ),
        function(
            "scroll",
            "Scroll the wheel at the pointer, or at the given screen coordinates. Positive scrolls down.",
            json!({
                "type": "object",
                "properties": {
                    "clicks": { "type": "integer", "description": "Wheel notches, -10 to 10, positive is down" },
                    "x": { "type": "integer", "description": "Move the mouse here first, X in screen pixels" },
                    "y": { "type": "integer", "description": "Move the mouse here first, Y in screen pixels" }
                },
                "required": ["clicks"]
            }),
        ),
        function(
            "screenshot",
            "Capture the whole screen and look at it. Use this whenever you need to see what is on screen, read an error, or find something to click.",
            json!({ "type": "object", "properties": {} }),
        ),
        function(
            "read_file",
            "Read a UTF-8 text file from your workspace. Paths are relative to the workspace and cannot escape it.",
            json!({
                "type": "object",
                "properties": { "path": { "type": "string", "description": "Path inside the workspace" } },
                "required": ["path"]
            }),
        ),
        function(
            "write_file",
            "Create or overwrite a UTF-8 text file in your workspace. Paths are relative to the workspace and cannot escape it.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path inside the workspace" },
                    "content": { "type": "string", "description": "Full file contents" }
                },
                "required": ["path", "content"]
            }),
        ),
        function(
            "list_files",
            "List one level of a directory inside your workspace. Use it to find a file before reading it.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Directory inside the workspace; omitted means the workspace root" }
                },
            }),
        ),
        function(
            "clipboard_get",
            "Read the text currently on the clipboard. Needs the user's approval: the clipboard may hold a password.",
            json!({ "type": "object", "properties": {} }),
        ),
        function(
            "clipboard_set",
            "Put text on the clipboard, for the user to paste.",
            json!({
                "type": "object",
                "properties": { "text": { "type": "string", "description": "Text to copy" } },
                "required": ["text"]
            }),
        ),
        function(
            "system_info",
            "Report the operating system, available capabilities and workspace path.",
            json!({ "type": "object", "properties": {} }),
        ),
    ];

    // A model that cannot take an image must not be offered the tool that
    // makes one: the provider rejects the whole turn when the screenshot
    // comes back as an image part.
    if !vision {
        schemas.retain(|schema| schema["function"]["name"] != "screenshot");
    }

    schemas
}

fn function(name: &str, description: &str, parameters: Value) -> Value {
    json!({
        "type": "function",
        "function": { "name": name, "description": description, "parameters": parameters }
    })
}

/// Short, human-readable description of a call - shown in the island's confirm
/// chip and written to the audit log.
pub fn summarise(name: &str, args: &Value) -> String {
    match name {
        "open_app" => format!("Open \u{201c}{}\u{201d}", str_arg(args, "name")),
        "close_app" => format!("Close \u{201c}{}\u{201d}", str_arg(args, "target")),
        "focus_window" => format!("Switch to \u{201c}{}\u{201d}", str_arg(args, "target")),
        "list_windows" => "List open windows".to_string(),
        "press_keys" => format!("Press {}", str_arg(args, "combo")),
        "screenshot" => "Take a screenshot and look at it".to_string(),
        "system_info" => "Check system information".to_string(),
        "type_text" => {
            let text = str_arg(args, "text");
            let count = text.chars().count();
            let preview: String = text.chars().take(48).collect();
            let ellipsis = if count > 48 { "…" } else { "" };
            format!("Type {count} characters: \u{201c}{preview}{ellipsis}\u{201d}")
        }
        "click" => format!(
            "Click at {}, {}",
            args.get("x").and_then(Value::as_i64).unwrap_or(0),
            args.get("y").and_then(Value::as_i64).unwrap_or(0)
        ),
        "wait" => match (
            args.get("ms").and_then(Value::as_i64),
            args.get("window").and_then(Value::as_str),
        ) {
            (Some(ms), _) => format!("Wait {:.1} s", ms as f64 / 1000.0),
            (None, Some(window)) => format!("Wait for \u{201c}{window}\u{201d}"),
            _ => "Wait".to_string(),
        },
        "scroll" => {
            let clicks = args.get("clicks").and_then(Value::as_i64).unwrap_or(0);
            let direction = if clicks < 0 { "up" } else { "down" };
            format!("Scroll {direction} {}", clicks.abs())
        }
        "list_files" => {
            let path = str_arg(args, "path");
            let shown = if path.trim().is_empty() { "." } else { path };
            format!("List files in {shown}")
        }
        "clipboard_get" => "Read the clipboard".to_string(),
        "clipboard_set" => {
            format!("Copy {} characters", str_arg(args, "text").chars().count())
        }
        "read_file" => format!("Read {}", str_arg(args, "path")),
        "write_file" => format!(
            "Write {} bytes to {}",
            str_arg(args, "content").len(),
            str_arg(args, "path")
        ),
        other => format!("Run {other}"),
    }
}

pub async fn execute(name: &str, args: &Value, ctx: &ToolContext) -> Result<ToolOutcome> {
    let platform = ctx.platform.clone();

    match name {
        "open_app" => {
            let app = need_str(args, "name")?;
            let text = platform.open_app(app).await?;
            Ok(done(text, summarise(name, args)))
        }
        "close_app" => {
            let target = need_str(args, "target")?;
            let text = platform.close_app(target).await?;
            Ok(done(text, summarise(name, args)))
        }
        "focus_window" => {
            let target = need_str(args, "target")?;
            let text = platform.focus_window(target).await?;
            Ok(done(text, summarise(name, args)))
        }
        "list_windows" => {
            let text = platform.list_windows().await?;
            Ok(done(text, summarise(name, args)))
        }
        "type_text" => {
            let text = need_str(args, "text")?;
            let result = platform.type_text(text).await?;
            Ok(done(
                if result.trim().is_empty() {
                    "typed".to_string()
                } else {
                    result
                },
                summarise(name, args),
            ))
        }
        "press_keys" => {
            let combo = need_str(args, "combo")?;
            platform.press_keys(combo).await?;
            Ok(done("pressed".to_string(), summarise(name, args)))
        }
        "click" => {
            let x = need_i32(args, "x")?;
            let y = need_i32(args, "y")?;
            platform.click(x, y).await?;
            Ok(done("clicked".to_string(), summarise(name, args)))
        }
        "wait" => {
            let ms = args.get("ms").and_then(Value::as_i64);
            let window = args
                .get("window")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|title| !title.is_empty());

            match (ms, window) {
                (Some(_), Some(_)) => bail!("give either ms or window, not both"),
                (None, None) => bail!("give ms or window"),
                (Some(ms), None) => {
                    if !(1..=10_000).contains(&ms) {
                        bail!("ms must be between 1 and 10000");
                    }
                    tokio::time::sleep(Duration::from_millis(ms as u64)).await;
                    Ok(done(
                        format!("waited {:.1} s", ms as f64 / 1000.0),
                        summarise(name, args),
                    ))
                }
                (None, Some(window)) => {
                    let timeout = args
                        .get("timeout_ms")
                        .and_then(Value::as_i64)
                        .unwrap_or(5000)
                        .clamp(100, 10_000);
                    let waited = wait_for_window(
                        &platform,
                        window,
                        Duration::from_millis(timeout as u64),
                    )
                    .await?;
                    Ok(done(
                        format!("waited {waited} s for a window matching \u{201c}{window}\u{201d}"),
                        summarise(name, args),
                    ))
                }
            }
        }
        "scroll" => {
            let clicks = need_i32(args, "clicks")?;
            if clicks == 0 || clicks.abs() > 10 {
                bail!("clicks must be between -10 and 10, and not zero");
            }
            let position = match (args.get("x"), args.get("y")) {
                (Some(_), Some(_)) => Some((need_i32(args, "x")?, need_i32(args, "y")?)),
                (None, None) => None,
                _ => bail!("give both x and y, or neither"),
            };
            platform.scroll(clicks, position).await?;
            let direction = if clicks < 0 { "up" } else { "down" };
            Ok(done(
                format!("scrolled {direction} {}", clicks.abs()),
                summarise(name, args),
            ))
        }
        "screenshot" => {
            if !ctx.vision {
                bail!("screenshots are disabled: this model cannot take an image");
            }
            let image = platform.screenshot().await?;
            Ok(ToolOutcome {
                text: "Screenshot attached to the conversation. Look at it before acting."
                    .to_string(),
                summary: summarise(name, args),
                image: Some(image),
            })
        }
        "read_file" => {
            let path_arg = need_str(args, "path")?;
            let path = resolve_in_workspace(&ctx.workspace, path_arg)
                .with_context(|| format!("{path_arg:?} is outside the workspace"))?;

            let metadata = tokio::fs::metadata(&path)
                .await
                .with_context(|| format!("could not stat {path_arg}"))?;
            if metadata.len() > MAX_READ_BYTES {
                bail!(
                    "{path_arg} is {} bytes; the read limit is {MAX_READ_BYTES}",
                    metadata.len()
                );
            }

            let bytes = tokio::fs::read(&path)
                .await
                .with_context(|| format!("could not read {path_arg}"))?;
            let text = String::from_utf8(bytes)
                .with_context(|| format!("{path_arg} is not UTF-8 text"))?;
            Ok(done(text, summarise(name, args)))
        }
        "write_file" => {
            let path_arg = need_str(args, "path")?;
            let content = need_str(args, "content")?;
            let path = resolve_in_workspace(&ctx.workspace, path_arg)
                .with_context(|| format!("{path_arg:?} is outside the workspace"))?;

            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .with_context(|| format!("could not create {}", parent.display()))?;
            }
            tokio::fs::write(&path, content.as_bytes())
                .await
                .with_context(|| format!("could not write {path_arg}"))?;

            Ok(done(
                format!("wrote {} bytes to {}", content.len(), path_arg),
                summarise(name, args),
            ))
        }
        "list_files" => {
            let raw = args.get("path").and_then(Value::as_str).unwrap_or("");
            let shown = if raw.trim().is_empty() { "." } else { raw };
            let dir = resolve_in_workspace(&ctx.workspace, shown)
                .with_context(|| format!("{shown:?} is outside the workspace"))?;

            let mut reader = tokio::fs::read_dir(&dir)
                .await
                .with_context(|| format!("could not list {shown}"))?;

            let mut entries: Vec<(String, bool, u64)> = Vec::new();
            while let Some(entry) = reader.next_entry().await? {
                let Ok(metadata) = entry.metadata().await else {
                    continue;
                };
                entries.push((
                    entry.file_name().to_string_lossy().to_string(),
                    metadata.is_dir(),
                    metadata.len(),
                ));
            }
            // Directories first, then names: `read_dir` order is whatever the
            // filesystem happens to return, which no model can rely on.
            entries.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase())));

            if entries.is_empty() {
                return Ok(done(
                    format!("{shown} is empty"),
                    summarise(name, args),
                ));
            }

            let total = entries.len();
            let mut text = format!("{total} entries in {shown}:");
            for (file, is_dir, bytes) in entries.iter().take(MAX_LIST_ENTRIES) {
                text.push('\n');
                if *is_dir {
                    text.push_str(&format!("{file}/"));
                } else {
                    text.push_str(&format!("{file} ({})", human_size(*bytes)));
                }
            }
            if total > MAX_LIST_ENTRIES {
                text.push_str(&format!("\n... {} more not shown", total - MAX_LIST_ENTRIES));
            }
            Ok(done(text, summarise(name, args)))
        }
        "clipboard_get" => {
            let text = platform.get_clipboard().await?;
            if text.trim().is_empty() {
                return Ok(done(
                    "(the clipboard is empty)".to_string(),
                    summarise(name, args),
                ));
            }
            let limit = MAX_READ_BYTES as usize;
            let clipped: String = text.chars().take(limit).collect();
            let note = if text.chars().count() > limit {
                "\n… (truncated)"
            } else {
                ""
            };
            Ok(done(format!("{}{note}", clipped.trim_end()), summarise(name, args)))
        }
        "clipboard_set" => {
            let text = need_str(args, "text")?;
            if text.len() > MAX_CLIPBOARD_BYTES {
                bail!(
                    "that text is {} bytes; the clipboard limit is {MAX_CLIPBOARD_BYTES}",
                    text.len()
                );
            }
            platform.set_clipboard(text).await?;
            Ok(done(
                format!("copied {} characters", text.chars().count()),
                summarise(name, args),
            ))
        }
        "system_info" => {
            let text = format!(
                "OS: {} ({})\nDesktop backend: {}\nWorkspace: {}\n\
                 Capabilities: open/close/focus windows, type text, press keys, click, screenshot, read/write files",
                std::env::consts::OS,
                std::env::consts::ARCH,
                platform.name(),
                ctx.workspace.display()
            );
            Ok(done(text, summarise(name, args)))
        }
        other => bail!("unknown tool: {other}"),
    }
}

/// Poll `list_windows` until a title contains `needle`, or the timeout expires.
/// A timeout lists what is open: the useful failure is "it is called Discord.exe",
/// which lets the model correct itself instead of repeating the same guess.
async fn wait_for_window(
    platform: &Arc<dyn Platform>,
    needle: &str,
    timeout: Duration,
) -> Result<String> {
    let needle = needle.to_lowercase();
    let started = Instant::now();
    loop {
        let windows = platform.list_windows().await?;
        if windows.to_lowercase().contains(&needle) {
            return Ok(format!("{:.1}", started.elapsed().as_secs_f32()));
        }
        if started.elapsed() >= timeout {
            let listing = windows
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .take(8)
                .collect::<Vec<_>>()
                .join(" | ");
            bail!(
                "no window matching {needle:?} appeared within {} ms; open windows: {listing}",
                timeout.as_millis()
            );
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

fn human_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

fn done(text: String, summary: String) -> ToolOutcome {
    ToolOutcome {
        text,
        summary,
        image: None,
    }
}

fn need_str<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    match args.get(key).and_then(Value::as_str) {
        Some(value) if !value.trim().is_empty() => Ok(value),
        _ => bail!("missing required string argument {key:?}"),
    }
}

fn need_i32(args: &Value, key: &str) -> Result<i32> {
    match args.get(key).and_then(Value::as_i64) {
        Some(value) => i32::try_from(value).context("coordinate out of range"),
        None => bail!("missing required integer argument {key:?}"),
    }
}

fn str_arg<'a>(args: &'a Value, key: &str) -> &'a str {
    args.get(key).and_then(Value::as_str).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform;
    use crate::platform::{Platform, PlatformFuture};
    use std::sync::Mutex;

    fn ctx() -> ToolContext {
        ToolContext {
            workspace: std::env::temp_dir().join("jarvis-tool-tests"),
            platform: platform::current(),
            vision: true,
        }
    }

    #[test]
    fn every_tool_has_a_schema_and_a_summary() {
        let schemas = schema_values(true);
        assert_eq!(schemas.len(), 16);

        for schema in &schemas {
            let name = schema["function"]["name"].as_str().unwrap();
            let summary = summarise(name, &json!({}));
            assert!(!summary.is_empty(), "no summary for {name}");
            assert!(
                schema["function"]["parameters"]["type"] == "object",
                "bad schema for {name}"
            );
        }
    }

    #[test]
    fn vision_disabled_hides_the_screenshot_tool() {
        let names = |vision: bool| {
            schema_values(vision)
                .iter()
                .map(|schema| schema["function"]["name"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        };

        assert!(names(true).iter().any(|name| name == "screenshot"));
        assert!(!names(false).iter().any(|name| name == "screenshot"));
    }

    #[test]
    fn summaries_are_human_readable() {
        assert_eq!(
            summarise("open_app", &json!({ "name": "Notepad" })),
            "Open \u{201c}Notepad\u{201d}"
        );
        assert_eq!(
            summarise("press_keys", &json!({ "combo": "ctrl+s" })),
            "Press ctrl+s"
        );
        assert!(summarise("type_text", &json!({ "text": "hello" })).contains("5 characters"));
        assert_eq!(
            summarise("click", &json!({ "x": 12, "y": 34 })),
            "Click at 12, 34"
        );
    }

    #[test]
    fn missing_arguments_are_reported() {
        assert!(need_str(&json!({}), "name").is_err());
        assert!(need_str(&json!({ "name": "   " }), "name").is_err());
        assert!(need_i32(&json!({ "x": "nope" }), "x").is_err());
        assert_eq!(need_i32(&json!({ "x": 5 }), "x").unwrap(), 5);
    }

    #[tokio::test]
    async fn files_round_trip_inside_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext {
            workspace: dir.path().to_path_buf(),
            platform: platform::current(),
            vision: true,
        };

        let write = execute(
            "write_file",
            &json!({ "path": "notes/today.txt", "content": "hello" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(write.text.contains("5 bytes"));

        let read = execute("read_file", &json!({ "path": "notes/today.txt" }), &ctx)
            .await
            .unwrap();
        assert_eq!(read.text, "hello");
    }

    #[tokio::test]
    async fn file_tools_refuse_to_escape_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext {
            workspace: dir.path().to_path_buf(),
            platform: platform::current(),
            vision: true,
        };

        assert!(execute(
            "write_file",
            &json!({ "path": "../escape.txt", "content": "x" }),
            &ctx
        )
        .await
        .is_err());
        assert!(
            execute("read_file", &json!({ "path": "/etc/hostname" }), &ctx)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn unknown_tools_error() {
        assert!(execute("run_shell", &json!({}), &ctx()).await.is_err());
    }

    /// Scripted desktop for the tools that need one. Windows only matters
    /// for the real backend, so the tests stay platform independent.
    type Scrolls = Vec<(i32, Option<(i32, i32)>)>;

    struct FakePlatform {
        scrolls: Mutex<Scrolls>,
        clipboard: Mutex<String>,
        windows: Mutex<Vec<String>>,
    }

    impl FakePlatform {
        fn new(windows: Vec<&str>) -> Arc<Self> {
            Arc::new(Self {
                scrolls: Mutex::new(Vec::new()),
                clipboard: Mutex::new(String::new()),
                windows: Mutex::new(windows.into_iter().map(str::to_string).collect()),
            })
        }
    }

    fn fake_ctx(platform: &Arc<FakePlatform>, workspace: PathBuf) -> ToolContext {
        ToolContext {
            workspace,
            platform: platform.clone(),
            vision: true,
        }
    }

    impl Platform for FakePlatform {
        fn name(&self) -> &'static str {
            "Fake"
        }
        fn open_app<'a>(&'a self, _name: &'a str) -> PlatformFuture<'a> {
            unimplemented!()
        }
        fn close_app<'a>(&'a self, _target: &'a str) -> PlatformFuture<'a> {
            unimplemented!()
        }
        fn focus_window<'a>(&'a self, _target: &'a str) -> PlatformFuture<'a> {
            unimplemented!()
        }
        fn list_windows(&self) -> PlatformFuture<'_> {
            let mut guard = self.windows.lock().unwrap();
            if guard.len() > 1 {
                guard.remove(0);
            }
            let answer = guard.first().cloned().unwrap_or_default();
            Box::pin(async move { Ok(answer) })
        }
        fn type_text<'a>(&'a self, _text: &'a str) -> PlatformFuture<'a> {
            unimplemented!()
        }
        fn press_keys<'a>(&'a self, _combo: &'a str) -> PlatformFuture<'a> {
            unimplemented!()
        }
        fn click(&self, _x: i32, _y: i32) -> PlatformFuture<'_> {
            unimplemented!()
        }
        fn screenshot(&self) -> PlatformFuture<'_> {
            unimplemented!()
        }
        fn scroll(&self, clicks: i32, position: Option<(i32, i32)>) -> PlatformFuture<'_> {
            self.scrolls.lock().unwrap().push((clicks, position));
            Box::pin(async { Ok(String::new()) })
        }
        fn get_clipboard(&self) -> PlatformFuture<'_> {
            let text = self.clipboard.lock().unwrap().clone();
            Box::pin(async move { Ok(text) })
        }
        fn set_clipboard<'a>(&'a self, text: &'a str) -> PlatformFuture<'a> {
            *self.clipboard.lock().unwrap() = text.to_string();
            Box::pin(async { Ok(String::new()) })
        }
    }

    #[tokio::test]
    async fn wait_rejects_both_or_neither_mode() {
        let platform = FakePlatform::new(vec![]);
        let ctx = fake_ctx(&platform, std::env::temp_dir());
        assert!(execute("wait", &json!({}), &ctx).await.is_err());
        assert!(execute("wait", &json!({ "ms": 50, "window": "x" }), &ctx)
            .await
            .is_err());
        assert!(execute("wait", &json!({ "ms": 0 }), &ctx).await.is_err());
    }

    #[tokio::test]
    async fn wait_for_window_returns_when_the_title_appears() {
        let platform = FakePlatform::new(vec!["", "Discord | Discord"]);
        let ctx = fake_ctx(&platform, std::env::temp_dir());
        let outcome = execute(
            "wait",
            &json!({ "window": "discord", "timeout_ms": 2000 }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(outcome.text.contains("waited"), "{}", outcome.text);
        assert!(outcome.text.contains("discord"), "{}", outcome.text);
    }

    #[tokio::test]
    async fn wait_for_window_timeout_lists_open_windows() {
        let platform = FakePlatform::new(vec!["Spotify | Spotify"]);
        let ctx = fake_ctx(&platform, std::env::temp_dir());
        let error = execute(
            "wait",
            &json!({ "window": "Discord", "timeout_ms": 200 }),
            &ctx,
        )
        .await
        .err()
        .expect("the wait should time out");
        let message = format!("{error:#}");
        assert!(message.contains("Spotify"), "{message}");
    }

    #[tokio::test]
    async fn list_files_is_jailed_to_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("notes.txt"), b"hi").unwrap();
        let platform = FakePlatform::new(vec![]);
        let ctx = fake_ctx(&platform, dir.path().to_path_buf());

        let listing = execute("list_files", &json!({}), &ctx).await.unwrap();
        assert!(listing.text.contains("notes.txt"), "{}", listing.text);

        assert!(execute("list_files", &json!({ "path": "../" }), &ctx)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn list_files_caps_the_listing() {
        let dir = tempfile::tempdir().unwrap();
        for index in 0..205 {
            std::fs::write(dir.path().join(format!("f{index:03}.txt")), b"x").unwrap();
        }
        let platform = FakePlatform::new(vec![]);
        let ctx = fake_ctx(&platform, dir.path().to_path_buf());
        let listing = execute("list_files", &json!({}), &ctx).await.unwrap();
        assert!(listing.text.contains("205 entries"), "{}", listing.text);
        assert!(listing.text.contains("5 more not shown"), "{}", listing.text);
    }

    #[tokio::test]
    async fn clipboard_text_reaches_the_platform_and_the_count_is_reported() {
        let platform = FakePlatform::new(vec![]);
        let ctx = fake_ctx(&platform, std::env::temp_dir());
        let outcome = execute("clipboard_set", &json!({ "text": "hello" }), &ctx)
            .await
            .unwrap();
        assert_eq!(*platform.clipboard.lock().unwrap(), "hello");
        assert_eq!(outcome.text, "copied 5 characters");

        let read = execute("clipboard_get", &json!({}), &ctx).await.unwrap();
        assert_eq!(read.text, "hello");
    }

    #[tokio::test]
    async fn scroll_needs_a_nonzero_click_count() {
        let platform = FakePlatform::new(vec![]);
        let ctx = fake_ctx(&platform, std::env::temp_dir());
        execute("scroll", &json!({ "clicks": -3 }), &ctx).await.unwrap();
        execute("scroll", &json!({ "clicks": 2, "x": 10, "y": 20 }), &ctx)
            .await
            .unwrap();
        assert_eq!(
            *platform.scrolls.lock().unwrap(),
            vec![(-3, None), (2, Some((10, 20)))]
        );

        assert!(execute("scroll", &json!({ "clicks": 0 }), &ctx).await.is_err());
        assert!(execute("scroll", &json!({ "clicks": 2, "x": 10 }), &ctx)
            .await
            .is_err());
    }
}
