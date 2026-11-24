use std::path::Path;

use serde_json::Value;

use crate::config::resolve_in_workspace;

/// What the user has to do before a tool call runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Read-only, no side effects.
    Safe,
    /// Changes something on the machine - needs an explicit Allow.
    Confirm,
    /// Never runs.
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub verdict: Verdict,
    pub reason: String,
}

impl Decision {
    fn new(verdict: Verdict, reason: impl Into<String>) -> Self {
        Self {
            verdict,
            reason: reason.into(),
        }
    }
}

/// Tools with no side effects at all.
const READ_ONLY: &[&str] = &["list_windows", "screenshot", "read_file", "system_info"];

/// Tools that change something on the machine.
const MUTATING: &[&str] = &[
    "open_app",
    "close_app",
    "focus_window",
    "type_text",
    "press_keys",
    "click",
    "write_file",
];

/// Never touched, even with confirmation: an agent that can be steered by
/// whatever it reads on screen has no business in a password manager.
const SENSITIVE: &[&str] = &[
    "keepass",
    "1password",
    "bitwarden",
    "lastpass",
    "dashlane",
    "authenticator",
    "metamask",
    "coinbase",
    "wallet",
    "bank",
    "vault",
    "password",
];

/// Decide what happens before a tool call reaches the OS.
pub fn evaluate(tool: &str, args: &Value, workspace: &Path) -> Decision {
    if READ_ONLY.contains(&tool) {
        if tool == "read_file" && resolve_in_workspace(workspace, str_arg(args, "path")).is_none() {
            return Decision::new(Verdict::Blocked, "path is outside the Jarvis workspace");
        }
        return Decision::new(Verdict::Safe, "read-only");
    }

    if tool == "write_file" && resolve_in_workspace(workspace, str_arg(args, "path")).is_none() {
        return Decision::new(Verdict::Blocked, "path is outside the Jarvis workspace");
    }

    if let Some(target) = target_name(tool, args) {
        let lower = target.to_lowercase();
        if let Some(hit) = SENSITIVE.iter().find(|needle| lower.contains(*needle)) {
            return Decision::new(
                Verdict::Blocked,
                format!("refuses to touch applications that look like {hit:?}"),
            );
        }
    }

    if MUTATING.contains(&tool) {
        return Decision::new(Verdict::Confirm, "changes something on your machine");
    }

    Decision::new(Verdict::Blocked, "unknown tool")
}

/// The app or window a tool would act on, if any.
fn target_name<'a>(tool: &str, args: &'a Value) -> Option<&'a str> {
    match tool {
        "open_app" => args.get("name").and_then(Value::as_str),
        "close_app" | "focus_window" => args
            .get("target")
            .or_else(|| args.get("title"))
            .or_else(|| args.get("name"))
            .and_then(Value::as_str),
        _ => None,
    }
}

fn str_arg<'a>(args: &'a Value, key: &str) -> &'a str {
    args.get(key).and_then(Value::as_str).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn workspace() -> &'static Path {
        Path::new("/tmp/jarvis-ws")
    }

    #[test]
    fn read_only_tools_run_without_asking() {
        for tool in ["list_windows", "screenshot", "system_info"] {
            let decision = evaluate(tool, &json!({}), workspace());
            assert_eq!(decision.verdict, Verdict::Safe, "{tool}");
        }
    }

    #[test]
    fn mutating_tools_need_confirmation() {
        let cases = [
            ("open_app", json!({ "name": "Notepad" })),
            ("close_app", json!({ "target": "chrome" })),
            ("type_text", json!({ "text": "hi" })),
            ("press_keys", json!({ "combo": "ctrl+s" })),
            ("click", json!({ "x": 10, "y": 20 })),
        ];
        for (tool, args) in cases {
            let decision = evaluate(tool, &args, workspace());
            assert_eq!(decision.verdict, Verdict::Confirm, "{tool}");
        }
    }

    #[test]
    fn password_managers_are_refused_outright() {
        let decision = evaluate("open_app", &json!({ "name": "1Password" }), workspace());
        assert_eq!(decision.verdict, Verdict::Blocked);

        let decision = evaluate("focus_window", &json!({ "target": "KeePass" }), workspace());
        assert_eq!(decision.verdict, Verdict::Blocked);
    }

    #[test]
    fn file_tools_cannot_escape_the_workspace() {
        let inside = evaluate("write_file", &json!({ "path": "notes.txt" }), workspace());
        assert_eq!(inside.verdict, Verdict::Confirm);

        let outside = evaluate(
            "write_file",
            &json!({ "path": "../../.ssh/id_rsa" }),
            workspace(),
        );
        assert_eq!(outside.verdict, Verdict::Blocked);

        let absolute = evaluate("read_file", &json!({ "path": "/etc/shadow" }), workspace());
        assert_eq!(absolute.verdict, Verdict::Blocked);
    }

    #[test]
    fn unknown_tools_are_blocked() {
        let decision = evaluate("run_shell", &json!({ "cmd": "rm -rf /" }), workspace());
        assert_eq!(decision.verdict, Verdict::Blocked);
    }
}
