//! Tool registry: schemas handed to the model, human summaries for the confirm
//! chip, and dispatch into the platform layer.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::config::resolve_in_workspace;
use crate::platform::Platform;

const MAX_READ_BYTES: u64 = 64 * 1024;

pub struct ToolContext {
    pub workspace: PathBuf,
    pub platform: Arc<dyn Platform>,
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
pub fn schema_values() -> Vec<Value> {
    vec![
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
            "system_info",
            "Report the operating system, available capabilities and workspace path.",
            json!({ "type": "object", "properties": {} }),
        ),
    ]
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
        "screenshot" => {
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

    fn ctx() -> ToolContext {
        ToolContext {
            workspace: std::env::temp_dir().join("jarvis-tool-tests"),
            platform: platform::current(),
        }
    }

    #[test]
    fn every_tool_has_a_schema_and_a_summary() {
        let schemas = schema_values();
        assert_eq!(schemas.len(), 11);

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
}
