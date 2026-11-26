use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Where dictation audio is turned into text. The text model answers the turn
/// either way; this only chooses who produces the transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SttProvider {
    /// Send the WAV to the configured audio-capable model.
    Model,
    /// Recognise it on this machine with the Windows speech engine.
    Windows,
    /// Recognise it locally with whisper.cpp, started as a child process.
    Whisper,
}

impl SttProvider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::Windows => "windows",
            Self::Whisper => "whisper",
        }
    }

    fn from_value(value: Option<String>) -> Self {
        match value
            .as_deref()
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("windows") => Self::Windows,
            Some("whisper") => Self::Whisper,
            // An unset or unrecognised value keeps the existing behaviour: a
            // silent switch to local recognition would be surprising.
            _ => Self::Model,
        }
    }
}

/// Runtime configuration, resolved at startup from the environment and from
/// `~/Alfred/settings.json` (written by the island's settings panel).
///
/// The API key never leaves the Rust process: it is never sent to the webview
/// and never written into a log.
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
    /// Optional `ALFRED_SITE_URL`, sent as `HTTP-Referer` to OpenRouter only.
    pub site_url: Option<String>,
    /// `ALFRED_STT_PROVIDER`: `model` (default), `windows` or `whisper`.
    pub stt_provider: SttProvider,
    /// `ALFRED_WHISPER_BIN`: whisper-cli command or Linux path inside WSL.
    pub whisper_bin: Option<String>,
    /// `ALFRED_WHISPER_MODEL`: Linux path to a GGUF model inside WSL.
    pub whisper_model: Option<String>,
    /// Optional `ALFRED_STT_MODEL`: transcribes voice when the main model
    /// cannot take audio. Falls back to `model` when unset.
    pub stt_model: Option<String>,
    /// Optional `ALFRED_STT_BASE_URL`: a different endpoint for voice
    /// transcription, so chat can run locally while voice stays in the cloud
    /// (or the reverse). Falls back to `base_url`.
    pub stt_base_url: Option<String>,
    /// Optional `ALFRED_STT_API_KEY`: the key for `stt_base_url`. Never falls
    /// back across hosts, so the chat key cannot leak to another provider.
    pub stt_api_key: Option<String>,
    /// Optional `ALFRED_FALLBACK_BASE_URL` / `ALFRED_FALLBACK_MODEL`: a local
    /// model used when the primary endpoint fails. Both are needed; see
    /// `Config::fallback`.
    pub fallback_base_url: Option<String>,
    pub fallback_model: Option<String>,
    /// Optional `ALFRED_FALLBACK_API_KEY`: only for a remote fallback, which is
    /// unusual - a loopback endpoint needs no key.
    pub fallback_api_key: Option<String>,
    /// `ALFRED_SPEAK`: read an answer aloud when the turn started by dictation.
    pub speak: bool,
    /// `ALFRED_VISION`: offer the screenshot tool. Off for a model whose
    /// endpoint rejects image input, so a turn cannot die on a screenshot.
    pub vision: bool,
    /// `ALFRED_VOICE`: SAPI voice name; empty means the system default.
    pub voice: String,
    /// `ALFRED_SPEECH_RATE`: SAPI rate, clamped to -10..=10.
    pub speech_rate: i32,
}

/// A second endpoint tried when the primary one fails.
#[derive(Debug, Clone)]
pub struct Fallback {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
}

impl Fallback {
    pub fn completions_url(&self) -> String {
        completions_url(&self.base_url)
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        let _ = dotenvy::dotenv();

        let resolved = resolve(&load_settings()?);

        let workspace = alfred_dir()?.join("workspace");
        std::fs::create_dir_all(&workspace)
            .with_context(|| format!("could not create workspace at {}", workspace.display()))?;

        let config = Self {
            base_url: resolved.base_url,
            api_key: resolved.api_key,
            model: resolved.model,
            max_steps: resolved.max_steps,
            workspace,
            site_url: resolved.site_url,
            stt_provider: resolved.stt_provider,
            whisper_bin: resolved.whisper_bin,
            whisper_model: resolved.whisper_model,
            stt_model: resolved.stt_model,
            stt_base_url: resolved.stt_base_url,
            stt_api_key: resolved.stt_api_key,
            fallback_base_url: resolved.fallback_base_url,
            fallback_model: resolved.fallback_model,
            fallback_api_key: resolved.fallback_api_key,
            speak: resolved.speak,
            vision: resolved.vision,
            voice: resolved.voice,
            speech_rate: resolved.speech_rate,
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
            stt_provider: SttProvider::Model,
            whisper_bin: None,
            whisper_model: None,
            stt_model: None,
            stt_base_url: None,
            stt_api_key: None,
            fallback_base_url: None,
            fallback_model: None,
            fallback_api_key: None,
            speak: true,
            vision: true,
            voice: String::new(),
            speech_rate: 0,
        }
    }

    /// `None` unless the endpoint *and* the model are set: half a fallback
    /// would silently run a model nobody chose.
    pub fn fallback(&self) -> Option<Fallback> {
        Some(Fallback {
            base_url: self.fallback_base_url.clone()?,
            api_key: self.fallback_api_key.clone().unwrap_or_default(),
            model: self.fallback_model.clone()?,
        })
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

    /// whisper-cli as seen from inside WSL: a command on PATH or a Linux path.
    pub fn whisper_binary(&self) -> String {
        self.whisper_bin
            .clone()
            .unwrap_or_else(|| "whisper-cli".to_string())
    }

    /// The model path must be a real Linux path: wsl.exe passes arguments
    /// without a shell, so `~` would never be expanded.
    pub fn whisper_model_path(&self) -> Result<String> {
        match &self.whisper_model {
            Some(path) if !path.trim().is_empty() => Ok(path.trim().to_string()),
            _ => anyhow::bail!(
                "no Whisper model configured. Set ALFRED_WHISPER_MODEL to the Linux path \
                 of a GGUF model inside WSL, e.g. /home/Chief/whisper.cpp/models/ggml-small.en.bin"
            ),
        }
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
                "no API key. Open Settings (the gear icon) and add one for an endpoint that \
                 supports tool calling and image input. A local endpoint such as Ollama \
                 (http://localhost:11434/v1) needs no key."
            );
        }
        if self.model.trim().is_empty() {
            anyhow::bail!("ALFRED_MODEL is not set.");
        }
        if !self.base_url.starts_with("http") {
            anyhow::bail!("ALFRED_BASE_URL must start with http:// or https://");
        }
        if let Some(fallback) = self.fallback() {
            if !fallback.base_url.starts_with("http") {
                anyhow::bail!("the fallback endpoint must start with http:// or https://");
            }
            if fallback.api_key.trim().is_empty() && !is_loopback(&fallback.base_url) {
                anyhow::bail!(
                    "the fallback endpoint {} is remote but has no key. Set ALFRED_FALLBACK_API_KEY, \
                     or point it at Ollama on loopback, which needs none",
                    fallback.base_url
                );
            }
        }
        if self.stt_provider == SttProvider::Whisper {
            self.whisper_model_path()?;
        }
        // Local recognition needs no voice endpoint or key at all.
        if self.stt_provider == SttProvider::Model
            && self.transcription_key().trim().is_empty()
            && !is_loopback(self.transcription_base_url())
        {
            anyhow::bail!(
                "voice transcription points at {} but has no key. Add a voice key in Settings \
                 (the chat key is not sent to a different endpoint).",
                self.transcription_base_url()
            );
        }
        Ok(())
    }

    /// `POST {base}/chat/completions`
    pub fn chat_completions_url(&self) -> String {
        completions_url(&self.base_url)
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

/// Values persisted by the settings panel in `~/Alfred/settings.json`.
///
/// Every field is optional: a value left out falls back to the environment and
/// then to the built-in default. Nothing here re-encrypts a key; it is plain
/// JSON in the user's home directory, the same trust level as `.env`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub model: Option<String>,
    pub max_steps: Option<usize>,
    pub site_url: Option<String>,
    pub stt_provider: Option<String>,
    pub whisper_bin: Option<String>,
    pub whisper_model: Option<String>,
    pub stt_model: Option<String>,
    pub stt_base_url: Option<String>,
    pub stt_api_key: Option<String>,
    pub fallback_base_url: Option<String>,
    pub fallback_model: Option<String>,
    pub fallback_api_key: Option<String>,
    pub speak: Option<bool>,
    pub vision: Option<bool>,
    pub voice: Option<String>,
    pub speech_rate: Option<i32>,
}

/// `Config`'s inputs after precedence, before validation.
///
/// Kept separate from `Config` so the settings panel can show and edit a
/// half-filled form that `validate` would reject (the first-run case).
#[derive(Debug, Clone)]
pub struct Resolved {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub max_steps: usize,
    pub site_url: Option<String>,
    pub stt_provider: SttProvider,
    pub whisper_bin: Option<String>,
    pub whisper_model: Option<String>,
    pub stt_model: Option<String>,
    pub stt_base_url: Option<String>,
    pub stt_api_key: Option<String>,
    pub fallback_base_url: Option<String>,
    pub fallback_model: Option<String>,
    pub fallback_api_key: Option<String>,
    pub speak: bool,
    pub vision: bool,
    pub voice: String,
    pub speech_rate: i32,
}

/// Current and previous variable prefixes. Both are read, with the new name
/// winning, so an existing `.env` or shell setup survives the rename.
const ENV_PREFIX: &str = "ALFRED_";
const LEGACY_ENV_PREFIX: &str = "JARVIS_";

fn legacy_name(name: &str) -> String {
    format!(
        "{LEGACY_ENV_PREFIX}{}",
        name.strip_prefix(ENV_PREFIX).unwrap_or(name)
    )
}

/// Variables that override the settings file.
pub const ENV_NAMES: [&str; 18] = [
    "ALFRED_BASE_URL",
    "ALFRED_API_KEY",
    "ALFRED_MODEL",
    "ALFRED_MAX_STEPS",
    "ALFRED_SITE_URL",
    "ALFRED_STT_PROVIDER",
    "ALFRED_WHISPER_BIN",
    "ALFRED_WHISPER_MODEL",
    "ALFRED_STT_MODEL",
    "ALFRED_STT_BASE_URL",
    "ALFRED_STT_API_KEY",
    "ALFRED_FALLBACK_BASE_URL",
    "ALFRED_FALLBACK_MODEL",
    "ALFRED_FALLBACK_API_KEY",
    "ALFRED_SPEAK",
    "ALFRED_VISION",
    "ALFRED_VOICE",
    "ALFRED_SPEECH_RATE",
];

/// Resolve settings exactly as `Config::load` does, but without validating.
pub fn resolve(settings: &Settings) -> Resolved {
    resolve_with(settings, optional_env)
}

/// The seam that keeps precedence testable without touching the real environment.
fn resolve_with(settings: &Settings, env: impl Fn(&str) -> Option<String>) -> Resolved {
    // Environment first, then the file, then the default: an existing `.env`
    // keeps working and the panel flags the fields it overrides. Each variable
    // is looked up under its new and its old name, in that order.
    let value = |name: &str, from_file: Option<String>| {
        env(name)
            .or_else(|| env(&legacy_name(name)))
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .or_else(|| {
                from_file
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty())
            })
    };

    Resolved {
        base_url: value("ALFRED_BASE_URL", settings.base_url.clone())
            .unwrap_or_else(|| "https://api.openai.com/v1".to_string())
            .trim_end_matches('/')
            .to_string(),
        api_key: value("ALFRED_API_KEY", settings.api_key.clone()).unwrap_or_default(),
        model: value("ALFRED_MODEL", settings.model.clone())
            .unwrap_or_else(|| "gpt-4o-mini".to_string())
            .trim()
            .to_string(),
        max_steps: value(
            "ALFRED_MAX_STEPS",
            settings.max_steps.map(|steps| steps.to_string()),
        )
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(8)
        .clamp(1, 24),
        site_url: value("ALFRED_SITE_URL", settings.site_url.clone()),
        stt_provider: SttProvider::from_value(value(
            "ALFRED_STT_PROVIDER",
            settings.stt_provider.clone(),
        )),
        whisper_bin: value("ALFRED_WHISPER_BIN", settings.whisper_bin.clone()),
        whisper_model: value("ALFRED_WHISPER_MODEL", settings.whisper_model.clone()),
        stt_model: value("ALFRED_STT_MODEL", settings.stt_model.clone()),
        stt_base_url: value("ALFRED_STT_BASE_URL", settings.stt_base_url.clone())
            .map(|value| value.trim_end_matches('/').to_string()),
        stt_api_key: value("ALFRED_STT_API_KEY", settings.stt_api_key.clone()),
        fallback_base_url: value(
            "ALFRED_FALLBACK_BASE_URL",
            settings.fallback_base_url.clone(),
        )
        .map(|value| value.trim_end_matches('/').to_string()),
        fallback_model: value("ALFRED_FALLBACK_MODEL", settings.fallback_model.clone()),
        fallback_api_key: value("ALFRED_FALLBACK_API_KEY", settings.fallback_api_key.clone()),
        speak: enabled(value(
            "ALFRED_SPEAK",
            settings.speak.map(|on| on.to_string()),
        )),
        vision: enabled(value(
            "ALFRED_VISION",
            settings.vision.map(|on| on.to_string()),
        )),
        voice: value("ALFRED_VOICE", settings.voice.clone()).unwrap_or_default(),
        speech_rate: value(
            "ALFRED_SPEECH_RATE",
            settings.speech_rate.map(|rate| rate.to_string()),
        )
        .and_then(|rate| rate.parse::<i32>().ok())
        .unwrap_or(0)
        .clamp(-10, 10),
    }
}

/// Only "off" spellings disable a switch, so an unset or odd value keeps the
/// default: a capability is turned off on purpose, never by accident.
fn enabled(value: Option<String>) -> bool {
    value
        .map(|flag| {
            !matches!(
                flag.to_ascii_lowercase().as_str(),
                "0" | "false" | "no" | "off"
            )
        })
        .unwrap_or(true)
}

fn completions_url(base_url: &str) -> String {
    format!("{}/chat/completions", base_url.trim_end_matches('/'))
}

/// Variables set in the environment (including `.env`): the panel shows those
/// fields read-only, because they win over anything it could save.
pub fn env_locked_names() -> Vec<&'static str> {
    ENV_NAMES
        .into_iter()
        .filter(|name| optional_env(name).is_some())
        .collect()
}

pub fn settings_path() -> Result<PathBuf> {
    Ok(alfred_dir()?.join("settings.json"))
}

pub fn load_settings() -> Result<Settings> {
    read_settings_at(&settings_path()?)
}

pub fn save_settings(settings: &Settings) -> Result<()> {
    write_settings_at(&settings_path()?, settings)
}

/// A missing file is an empty configuration; a corrupt one is an error, because
/// silently starting over would drop the saved key without saying so.
fn read_settings_at(path: &Path) -> Result<Settings> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Settings::default());
        }
        Err(error) => {
            return Err(error).with_context(|| format!("could not read {}", path.display()));
        }
    };
    serde_json::from_str(&text).with_context(|| {
        format!(
            "{} is not valid settings JSON; fix or delete it",
            path.display()
        )
    })
}

fn write_settings_at(path: &Path, settings: &Settings) -> Result<()> {
    let text = serde_json::to_string_pretty(settings).context("could not encode settings")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("could not create {}", parent.display()))?;
    }
    std::fs::write(path, text).with_context(|| format!("could not write {}", path.display()))
}

/// `~/Alfred` - workspace, audit log and settings live here. An existing
/// `~/Jarvis` is kept so its settings, transcript and approvals survive the
/// rename; only a fresh install creates `~/Alfred`.
pub fn alfred_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("could not locate the home directory")?;
    let dir = dir_for_home(&home);
    std::fs::create_dir_all(&dir).with_context(|| format!("could not create {}", dir.display()))?;
    Ok(dir)
}

fn dir_for_home(home: &Path) -> PathBuf {
    let current = home.join("Alfred");
    let legacy = home.join("Jarvis");
    if current.exists() || !legacy.exists() {
        current
    } else {
        legacy
    }
}

/// Parse a base URL for host checks; `ALFRED_BASE_URL` is validated at load, so
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

/// Read a variable, trimmed, treating an empty value as unset. The old name is
/// accepted too, so a field set in an existing `.env` is still shown read-only.
fn optional_env(name: &str) -> Option<String> {
    env::var(name)
        .or_else(|_| env::var(legacy_name(name)))
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

pub fn audit_log_path() -> Result<PathBuf> {
    Ok(alfred_dir()?.join("audit.log"))
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
        // The tools receive `Config::workspace` (e.g. `~/Alfred/workspace`),
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
        let root = Path::new("/tmp/alfred-definitely-not-created/ws");
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
    fn vision_is_on_by_default_and_only_an_explicit_off_disables_it() {
        assert!(resolve_with(&Settings::default(), |_| None).vision);

        let off = Settings {
            vision: Some(false),
            ..Settings::default()
        };
        assert!(!resolve_with(&off, |_| None).vision);
        assert!(
            resolve_with(&off, |name| (name == "ALFRED_VISION")
                .then(|| "on".to_string()))
            .vision,
            "the environment wins over the saved file"
        );
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
    fn windows_transcription_needs_no_voice_endpoint_or_key() {
        let mut config = Config::new("https://api.openai.com/v1", "sk-test", "m", "/tmp/ws");
        config.stt_base_url = Some("https://elsewhere.test/v1".into());
        assert!(config.validate().is_err(), "a remote voice endpoint needs a key");

        config.stt_provider = SttProvider::Windows;
        assert!(config.validate().is_ok(), "local recognition ignores voice settings");
        assert_eq!(config.stt_provider.as_str(), "windows");
    }

    #[test]
    fn stt_provider_resolves_with_precedence_and_a_safe_default() {
        let settings = Settings {
            stt_provider: Some("windows".into()),
            ..Settings::default()
        };
        assert_eq!(
            resolve_with(&settings, |_| None).stt_provider,
            SttProvider::Windows
        );
        assert_eq!(
            resolve_with(&settings, |name| {
                (name == "ALFRED_STT_PROVIDER").then(|| "model".to_string())
            })
            .stt_provider,
            SttProvider::Model,
            "the environment wins over the saved file"
        );
        assert_eq!(
            resolve_with(&Settings::default(), |_| None).stt_provider,
            SttProvider::Model
        );
        assert_eq!(
            SttProvider::from_value(Some("nonsense".into())),
            SttProvider::Model,
            "an odd value must not silently change providers"
        );
    }

    #[test]
    fn whisper_provider_uses_the_wsl_paths() {
        let settings = Settings {
            stt_provider: Some("whisper".into()),
            whisper_bin: Some("/home/Chief/whisper.cpp/build/bin/whisper-cli".into()),
            whisper_model: Some("/home/Chief/whisper.cpp/models/ggml-small.en.bin".into()),
            ..Settings::default()
        };
        let resolved = resolve_with(&settings, |_| None);
        assert_eq!(resolved.stt_provider, SttProvider::Whisper);

        let mut config = Config::new("http://localhost:11434/v1", "", "m", "/tmp/ws");
        config.stt_provider = SttProvider::Whisper;
        assert!(config.validate().is_err(), "a model path is required");
        config.whisper_bin = resolved.whisper_bin.clone();
        config.whisper_model = resolved.whisper_model.clone();
        assert!(config.validate().is_ok());
        assert_eq!(config.whisper_binary(), "/home/Chief/whisper.cpp/build/bin/whisper-cli");
        assert_eq!(
            config.whisper_model_path().unwrap(),
            "/home/Chief/whisper.cpp/models/ggml-small.en.bin"
        );
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

    #[test]
    fn a_missing_settings_file_is_an_empty_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let settings = read_settings_at(&path).unwrap();
        assert!(settings.base_url.is_none());
        assert!(settings.api_key.is_none());
    }

    #[test]
    fn settings_round_trip_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/settings.json");
        let settings = Settings {
            base_url: Some("https://openrouter.ai/api/v1".into()),
            api_key: Some("sk-or-v1-test".into()),
            model: Some("google/gemini-2.5-flash".into()),
            max_steps: Some(12),
            ..Settings::default()
        };

        write_settings_at(&path, &settings).unwrap();
        let loaded = read_settings_at(&path).unwrap();

        assert_eq!(loaded.base_url, settings.base_url);
        assert_eq!(loaded.api_key, settings.api_key);
        assert_eq!(loaded.model, settings.model);
        assert_eq!(loaded.max_steps, Some(12));
        assert!(loaded.stt_model.is_none());
    }

    #[test]
    fn a_corrupt_settings_file_is_reported_rather_than_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "{ not json").unwrap();
        assert!(read_settings_at(&path).is_err());
    }

    #[test]
    fn the_environment_beats_the_saved_file() {
        let settings = Settings {
            model: Some("saved-model".into()),
            ..Settings::default()
        };

        let from_env = resolve_with(&settings, |name| {
            (name == "ALFRED_MODEL").then(|| "env-model".to_string())
        });
        assert_eq!(from_env.model, "env-model");

        let from_file = resolve_with(&settings, |_| None);
        assert_eq!(from_file.model, "saved-model");
    }

    #[test]
    fn resolve_applies_defaults_and_clamps_saved_steps() {
        let resolved = resolve_with(
            &Settings {
                api_key: Some("   ".into()),
                max_steps: Some(999),
                ..Settings::default()
            },
            |_| None,
        );

        assert_eq!(resolved.base_url, "https://api.openai.com/v1");
        assert_eq!(resolved.model, "gpt-4o-mini");
        assert_eq!(resolved.max_steps, 24);
        assert_eq!(resolved.api_key, "", "blank is not a key");
    }

    #[test]
    fn a_saved_local_endpoint_still_needs_no_key() {
        let resolved = resolve_with(
            &Settings {
                base_url: Some("http://localhost:11434/v1/".into()),
                model: Some("qwen2.5vl".into()),
                ..Settings::default()
            },
            |_| None,
        );

        let config = Config::new(
            resolved.base_url,
            resolved.api_key,
            resolved.model,
            "/tmp/ws",
        );
        assert!(config.is_local());
        assert!(config.validate().is_ok());
    }

    #[test]
    fn legacy_environment_names_still_work() {
        let settings = Settings {
            model: Some("saved-model".into()),
            ..Settings::default()
        };

        let from_legacy = resolve_with(&settings, |name| {
            (name == "JARVIS_MODEL").then(|| "old-env-model".to_string())
        });
        assert_eq!(from_legacy.model, "old-env-model");

        let both = resolve_with(&settings, |name| match name {
            "ALFRED_MODEL" => Some("new-env-model".into()),
            "JARVIS_MODEL" => Some("old-env-model".into()),
            _ => None,
        });
        assert_eq!(both.model, "new-env-model", "the new name wins");
    }

    #[test]
    fn an_existing_legacy_directory_is_kept() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(dir_for_home(home.path()), home.path().join("Alfred"));

        std::fs::create_dir(home.path().join("Jarvis")).unwrap();
        assert_eq!(dir_for_home(home.path()), home.path().join("Jarvis"));

        std::fs::create_dir(home.path().join("Alfred")).unwrap();
        assert_eq!(dir_for_home(home.path()), home.path().join("Alfred"));
    }
}
