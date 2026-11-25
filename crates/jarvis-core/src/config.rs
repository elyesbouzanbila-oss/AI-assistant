use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Runtime configuration, read once at startup from the environment.
///
/// The API key lives only in this process: it is never sent to the webview and
/// never written into a log.
#[derive(Debug, Clone)]
pub struct Config {
    /// OpenAI-compatible base URL, e.g. `https://api.openai.com/v1`.
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    /// Hard cap on tool-call rounds per user turn.
    pub max_steps: usize,
    /// Sandbox root for file tools.
    pub workspace: PathBuf,
    /// Optional `JARVIS_SITE_URL`, sent as `HTTP-Referer` to OpenRouter only.
    pub site_url: Option<String>,
    /// Optional `JARVIS_STT_MODEL`: transcribes voice when the main model
    /// cannot take audio. Falls back to `model` when unset.
    pub stt_model: Option<String>,
    /// Optional `JARVIS_STT_BASE_URL`: a different endpoint for voice
    /// transcription, so chat can run locally while voice stays in the cloud
    /// (or the reverse). Falls back to `base_url`.
    pub stt_base_url: Option<String>,
    /// Optional `JARVIS_STT_API_KEY`: the key for `stt_base_url`. Never falls
    /// back across hosts, so the chat key cannot leak to another provider.
    pub stt_api_key: Option<String>,
}

impl Config {
    pub fn load() -> Result<Self> {
        let _ = dotenvy::dotenv();

        let base_url = env::var("JARVIS_BASE_URL")
            .unwrap_or_else(|_| "https://api.openai.com/v1".to_string())
            .trim_end_matches('/')
            .to_string();

        let api_key = env::var("JARVIS_API_KEY").unwrap_or_default();

        let model = env::var("JARVIS_MODEL")
            .unwrap_or_else(|_| "gpt-4o-mini".to_string())
            .trim()
            .to_string();

        let max_steps = env::var("JARVIS_MAX_STEPS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(8)
            .clamp(1, 24);

        let workspace = jarvis_dir()?.join("workspace");
        std::fs::create_dir_all(&workspace)
            .with_context(|| format!("could not create workspace at {}", workspace.display()))?;

        let config = Self {
            base_url,
            api_key,
            model,
            max_steps,
            workspace,
            site_url: env::var("JARVIS_SITE_URL").ok(),
            stt_model: optional_env("JARVIS_STT_MODEL"),
            stt_base_url: optional_env("JARVIS_STT_BASE_URL")
                .map(|value| value.trim_end_matches('/').to_string()),
            stt_api_key: optional_env("JARVIS_STT_API_KEY"),
        };
        config.validate()?;
        Ok(config)
    }

    /// Explicit constructor, used by tests and by embedders that already have
    /// values in hand.
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
        workspace: impl Into<PathBuf>,
    ) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            model: model.into(),
            max_steps: 8,
            workspace: workspace.into(),
            site_url: None,
            stt_model: None,
            stt_base_url: None,
            stt_api_key: None,
        }
    }

    /// Model used for voice transcription.
    pub fn transcription_model(&self) -> &str {
        self.stt_model.as_deref().unwrap_or(&self.model)
    }

    /// Endpoint used for voice transcription.
    pub fn transcription_base_url(&self) -> &str {
        self.stt_base_url.as_deref().unwrap_or(&self.base_url)
    }

    /// Key used for voice transcription. A separate endpoint needs its own key:
    /// the chat key is only reused when transcription shares that endpoint.
    pub fn transcription_key(&self) -> &str {
        match (&self.stt_base_url, &self.stt_api_key) {
            (None, None) => &self.api_key,
            (_, Some(key)) => key,
            (Some(_), None) => "",
        }
    }

    /// `POST {stt_base}/chat/completions`
    pub fn transcription_url(&self) -> String {
        format!("{}/chat/completions", self.transcription_base_url())
    }

    /// OpenRouter speaks the same protocol as OpenAI but wants a `provider/model`
    /// id and accepts attribution headers. Everything else is a plain
    /// OpenAI-compatible endpoint and must receive no OpenRouter headers.
    pub fn is_openrouter(&self) -> bool {
        endpoint(&self.base_url).host_str() == Some("openrouter.ai")
    }

    /// Loopback endpoints (Ollama and friends) need no credentials.
    pub fn is_local(&self) -> bool {
        is_loopback(&self.base_url)
    }

    pub fn validate(&self) -> Result<()> {
        if self.api_key.trim().is_empty() && !self.is_local() {
            anyhow::bail!(
                "JARVIS_API_KEY is not set. Copy .env.example to .env and add a key \
                 (OpenRouter by default) for an endpoint that supports tool calling and image input. \
                 A local endpoint such as Ollama (http://localhost:11434/v1) needs no key."
            );
        }
        if self.model.trim().is_empty() {
            anyhow::bail!("JARVIS_MODEL is not set.");
        }
        if !self.base_url.starts_with("http") {
            anyhow::bail!("JARVIS_BASE_URL must start with http:// or https://");
        }
        if self.transcription_key().trim().is_empty() && !is_loopback(self.transcription_base_url())
        {
            anyhow::bail!(
                "voice transcription points at {} but has no key. Set JARVIS_STT_API_KEY \
                 (the chat key is not sent to a different endpoint).",
                self.transcription_base_url()
            );
        }
        Ok(())
    }

    /// `POST {base}/chat/completions`
    pub fn chat_completions_url(&self) -> String {
        format!("{}/chat/completions", self.base_url)
    }

    /// Redacted summary, safe to log.
    pub fn describe(&self) -> String {
        format!(
            "{} @ {} (key: {})",
            self.model,
            self.base_url,
            self.masked_key()
        )
    }

    fn masked_key(&self) -> String {
        let key = self.api_key.trim();
        if key.is_empty() {
            "none".to_string()
        } else if key.len() <= 6 {
            "set".to_string()
        } else {
            let head: String = key.chars().take(4).collect();
            format!("{head}***")
        }
    }
}

/// `~/Jarvis` - workspace, audit log and future settings live here.
pub fn jarvis_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("could not locate the home directory")?;
    let dir = home.join("Jarvis");
    std::fs::create_dir_all(&dir).with_context(|| format!("could not create {}", dir.display()))?;
    Ok(dir)
}

/// Parse a base URL for host checks; `JARVIS_BASE_URL` is validated at load, so
/// this only fails for hand-built configs.
fn endpoint(base_url: &str) -> url::Url {
    url::Url::parse(base_url).unwrap_or_else(|_| url::Url::parse("http://invalid").unwrap())
}

/// A loopback host needs no API key.
fn is_loopback(base_url: &str) -> bool {
    matches!(
        endpoint(base_url).host_str(),
        Some("localhost" | "127.0.0.1" | "::1" | "[::1]")
    )
}

/// Read a variable, trimmed, treating an empty value as unset.
fn optional_env(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

pub fn audit_log_path() -> Result<PathBuf> {
    Ok(jarvis_dir()?.join("audit.log"))
}

/// Resolve `candidate` inside `workspace`, refusing anything that escapes it.
pub fn resolve_in_workspace(workspace: &Path, candidate: &str) -> Option<PathBuf> {
    let candidate = candidate.trim();
    if candidate.is_empty() {
        return None;
    }

    let path = Path::new(candidate);
    if path.components().any(|component| {
        matches!(
            component,
            std::path::Component::ParentDir | std::path::Component::Prefix(_)
        )
    }) {
        return None;
    }

    // Resolve the root first and build the candidate from it: `canonicalize`
    // returns a Windows verbatim path (`\\\\?\\C:\\...`) while joining onto the raw
    // workspace does not, and the two never compare equal.
    let root = workspace
        .canonicalize()
        .unwrap_or_else(|_| normalize(workspace));
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };

    // Prefer real paths when they exist (so a symlink cannot point outside),
    // and fall back to lexical normalisation for files that are not written yet.
    let normalized = joined.canonicalize().unwrap_or_else(|_| normalize(&joined));

    if normalized.starts_with(&root) {
        Some(normalized)
    } else {
        None
    }
}

/// Lexical normalisation without touching the filesystem.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_paths_are_jailed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();

        assert!(resolve_in_workspace(&root, "notes.txt").is_some());
        assert!(resolve_in_workspace(&root, "a/b/c.txt").is_some());
        assert!(resolve_in_workspace(&root, "./notes.txt").is_some());
        assert!(resolve_in_workspace(&root, "../escape.txt").is_none());
        assert!(resolve_in_workspace(&root, "a/../../escape.txt").is_none());
        assert!(resolve_in_workspace(&root, "/etc/passwd").is_none());
        assert!(resolve_in_workspace(&root, "").is_none());
    }

    #[test]
    fn new_files_are_allowed_when_the_workspace_is_not_canonicalized() {
        // The tools receive `Config::workspace` (e.g. `~/Jarvis/workspace`),
        // which is not canonical, while `canonicalize` on Windows returns a
        // `\\\\?\\` path. Both must still agree on what is inside the workspace.
        let dir = tempfile::tempdir().unwrap();
        assert!(resolve_in_workspace(dir.path(), "notes/today.txt").is_some());
        assert!(resolve_in_workspace(dir.path(), "a/b/c.txt").is_some());
        assert!(resolve_in_workspace(dir.path(), "../escape.txt").is_none());
    }

    #[test]
    fn workspace_jail_works_before_the_directory_exists() {
        // A missing workspace must not silently block every file tool.
        let root = Path::new("/tmp/jarvis-definitely-not-created/ws");
        assert!(resolve_in_workspace(root, "notes.txt").is_some());
        assert!(resolve_in_workspace(root, "../escape.txt").is_none());
    }

    #[test]
    fn config_requires_a_key() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::new("https://example.test/v1", "", "m", dir.path());
        assert!(config.validate().is_err());
        config.api_key = "sk-test".into();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn local_endpoints_need_no_key() {
        let mut config = Config::new("http://localhost:11434/v1", "", "qwen2.5vl", "/tmp/ws");
        assert!(config.is_local());
        assert!(config.validate().is_ok());

        config.base_url = "https://api.openai.com/v1".into();
        assert!(!config.is_local());
        assert!(config.validate().is_err());
    }

    #[test]
    fn loopback_forms_are_recognised() {
        for url in [
            "http://localhost:11434/v1",
            "http://127.0.0.1:11434/v1",
            "http://[::1]:11434/v1",
        ] {
            assert!(
                Config::new(url, "", "m", "/tmp/ws").is_local(),
                "{url} should count as local"
            );
        }
        assert!(!Config::new("https://openrouter.ai/api/v1", "k", "m", "/tmp/ws").is_local());
    }

    #[test]
    fn voice_can_use_a_different_endpoint_and_key() {
        let mut config = Config::new("http://localhost:11434/v1", "", "qwen2.5vl", "/tmp/ws");
        // Unset: voice follows chat, and loopback needs no key.
        assert_eq!(config.transcription_base_url(), "http://localhost:11434/v1");
        assert_eq!(config.transcription_key(), "");
        assert!(config.validate().is_ok());

        // A separate endpoint needs its own key; the chat key is never reused.
        config.stt_base_url = Some("https://api.groq.com/openai/v1".into());
        config.stt_model = Some("whisper-large-v3-turbo".into());
        assert!(config.validate().is_err());

        config.stt_api_key = Some("gsk-test".into());
        assert_eq!(config.transcription_key(), "gsk-test");
        assert_eq!(
            config.transcription_url(),
            "https://api.groq.com/openai/v1/chat/completions"
        );
        assert!(config.validate().is_ok());
    }

    #[test]
    fn voice_falls_back_to_the_chat_endpoint_and_key() {
        let mut config = Config::new("https://openrouter.ai/api/v1", "sk-or", "m", "/tmp/ws");
        config.stt_model = Some("google/gemini-2.5-flash".into());
        assert_eq!(
            config.transcription_base_url(),
            "https://openrouter.ai/api/v1"
        );
        assert_eq!(config.transcription_key(), "sk-or");
        assert!(config.validate().is_ok());
    }

    #[test]
    fn recognises_openrouter_endpoints() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::new(
            "https://openrouter.ai/api/v1",
            "sk-or-v1-test",
            "google/gemini-2.5-flash",
            dir.path(),
        );
        assert!(config.is_openrouter());
        assert_eq!(
            config.chat_completions_url(),
            "https://openrouter.ai/api/v1/chat/completions"
        );

        let other = Config::new("https://api.openai.com/v1", "k", "gpt-4o-mini", dir.path());
        assert!(!other.is_openrouter());
    }

    #[test]
    fn chat_url_has_no_double_slash() {
        let config = Config::new("https://example.test/v1/", "k", "m", "/tmp");
        assert_eq!(
            config.chat_completions_url(),
            "https://example.test/v1/chat/completions"
        );
    }

    #[test]
    fn transcription_model_falls_back_to_the_main_model() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::new(
            "https://openrouter.ai/api/v1",
            "k",
            "google/gemini-2.5-flash",
            dir.path(),
        );
        assert_eq!(config.transcription_model(), "google/gemini-2.5-flash");

        config.stt_model = Some("openai/gpt-4o-audio-preview".into());
        assert_eq!(config.transcription_model(), "openai/gpt-4o-audio-preview");
    }

    #[test]
    fn describe_masks_short_and_unicode_keys_without_panicking() {
        let dir = tempfile::tempdir().unwrap();
        for key in ["sk-123456", "sk-é-key"] {
            let config = Config::new("https://example.test/v1", key, "m", dir.path());
            let described = config.describe();
            assert!(described.contains("***"));
            assert!(!described.contains(key));
        }
    }
}
