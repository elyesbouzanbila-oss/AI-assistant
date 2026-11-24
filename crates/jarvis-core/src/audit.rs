use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

/// Append-only JSONL record of everything Jarvis did, so the user can always
/// reconstruct what happened on their machine.
pub struct Audit {
    path: Option<PathBuf>,
}

impl Audit {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: Some(path.into()),
        }
    }

    pub fn disabled() -> Self {
        Self { path: None }
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn record(&self, tool: &str, args: &Value, outcome: &str) {
        let Some(path) = &self.path else { return };

        let entry = json!({
            "ts": chrono::Local::now().to_rfc3339(),
            "tool": tool,
            "args": truncate(args, 400),
            "outcome": truncate(&Value::String(outcome.to_string()), 400),
        });

        let ok = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut file| writeln!(file, "{entry}"));

        if let Err(error) = ok {
            tracing::warn!("could not append to audit log {}: {error}", path.display());
        }
    }
}

fn truncate(value: &Value, max: usize) -> String {
    let text = match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let clean = text.replace(['\n', '\r'], " ");
    if clean.chars().count() <= max {
        clean
    } else {
        let head: String = clean.chars().take(max).collect();
        format!("{head}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_one_line_of_valid_json_per_action() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.log");
        let audit = Audit::new(&path);

        audit.record("open_app", &json!({ "name": "Notepad" }), "opened");
        audit.record("type_text", &json!({ "text": "hello\nworld" }), "typed");

        let body = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2);
        for line in lines {
            let parsed: Value = serde_json::from_str(line).unwrap();
            assert!(parsed.get("ts").is_some());
            assert!(parsed.get("tool").is_some());
        }
        assert!(!body.contains("hello\nworld"));
    }

    #[test]
    fn disabled_audit_writes_nothing() {
        let audit = Audit::disabled();
        audit.record("open_app", &json!({}), "noop");
        assert!(audit.path().is_none());
    }
}
