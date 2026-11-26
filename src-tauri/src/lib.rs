//! Tauri shell for Jarvis.
//!
//! All real work happens in `jarvis-core`; this file only wires that into a
//! window: commands in, events out, plus the global shortcut.

mod island;
mod ollama;
mod speech;

use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Arc;

use jarvis_core::agent::{Agent, AgentEvent, Emitter, Phase};
use jarvis_core::audit::Audit;
use jarvis_core::config::{self, Config, SttProvider};
use jarvis_core::llm::LlmClient;
use jarvis_core::platform;
use jarvis_core::platform::recorder::Recorder;
use tauri::{AppHandle, Emitter as _, Manager, State, WebviewWindow};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};

const NOT_CONFIGURED: &str =
    "Jarvis has no model yet. Open Settings (the gear icon) and add an endpoint, model and key.";

/// Speech settings plus the per-turn flag. Shared by the event forwarder, which
/// speaks, and the commands, which know how the turn started; `save_settings`
/// refreshes it with the rest of the runtime.
#[derive(Clone, Default)]
struct VoiceState {
    turn: Arc<AtomicBool>,
    enabled: Arc<AtomicBool>,
    voice: Arc<std::sync::Mutex<String>>,
    rate: Arc<AtomicI32>,
}

impl VoiceState {
    fn update(&self, config: &Config) {
        self.enabled.store(config.speak, Ordering::SeqCst);
        if let Ok(mut name) = self.voice.lock() {
            *name = config.voice.clone();
        }
        self.rate.store(config.speech_rate, Ordering::SeqCst);
    }

    /// Speak a finished turn, if it started by dictation. The flag is consumed
    /// either way, so one answer is spoken at most once.
    fn answer(&self, text: &str) {
        let by_voice = self.turn.swap(false, Ordering::SeqCst);
        if !by_voice || !self.enabled.load(Ordering::SeqCst) {
            return;
        }
        let Some(spoken) = jarvis_core::speech::to_speech(text) else {
            return;
        };
        let name = self
            .voice
            .lock()
            .map(|voice| voice.clone())
            .unwrap_or_default();

        // Silence is an acceptable outcome; a turn that dies because the sound
        // card was busy is not.
        if let Err(error) = speech::speak(&spoken, &name, self.rate.load(Ordering::SeqCst)) {
            tracing::warn!("could not speak the answer: {error}");
        }
    }
}

struct AppState {
    /// Rebuilt by `save_settings`, so a new endpoint or key applies at once.
    runtime: std::sync::Mutex<Runtime>,
    /// The microphone while a dictation is in progress.
    recorder: tokio::sync::Mutex<Option<Recorder>>,
    /// Kept so a rebuilt agent still reports to the same island.
    emit: Emitter,
    /// Whether the current turn was dictated, and how to speak it.
    voice: VoiceState,
}

struct Runtime {
    agent: Option<Arc<Agent>>,
    /// Shares the agent's endpoint and key; used only for voice transcription.
    llm: Option<Arc<LlmClient>>,
    /// Where dictation audio is transcribed; read by `stop_dictation`.
    stt_provider: SttProvider,
    /// whisper-cli command and Linux model path inside WSL.
    whisper: Option<(String, String)>,
    /// Set when the configuration was rejected, e.g. a missing API key.
    startup_error: Option<String>,
}

impl Runtime {
    fn build(emit: &Emitter, voice: &VoiceState) -> Self {
        match Config::load() {
            Ok(config) => {
                tracing::info!("starting with {}", config.describe());
                voice.update(&config);
                let audit = config::audit_log_path()
                    .map(Audit::new)
                    .unwrap_or_else(|_| Audit::disabled());
                let mut client = LlmClient::new(&config);
                let fallback = config.fallback();
                if let Some(fallback) = &fallback {
                    tracing::info!("falling back to {} at {}", fallback.model, fallback.base_url);
                    client = client.with_fallback(fallback);
                }

                // Wake whichever loopback endpoint can answer. A local primary
                // needs this too: without a fallback there is no other retry.
                let (wake_endpoint, wake_model, wake_primary) = if config.is_local() {
                    (config.base_url.clone(), config.model.clone(), true)
                } else if let Some(fallback) = &fallback {
                    (fallback.base_url.clone(), fallback.model.clone(), false)
                } else {
                    (String::new(), String::new(), false)
                };
                if !wake_endpoint.is_empty() {
                    let wake = Arc::new(move || ollama::ensure(&wake_endpoint, &wake_model));
                    client = if wake_primary {
                        client.with_primary_wake(wake)
                    } else {
                        client.with_wake(wake)
                    };
                }

                // The agent must use this same client: it is the one carrying
                // the fallback and wake hook. Building another one here would
                // silently drop both for every conversation turn.
                let llm = Arc::new(client);
                let stt_provider = config.stt_provider;
                let whisper = if stt_provider == SttProvider::Whisper {
                    config
                        .whisper_model_path()
                        .ok()
                        .map(|model| (config.whisper_binary(), model))
                } else {
                    None
                };
                let agent = Agent::new(
                    config,
                    platform::current(),
                    llm.clone(),
                    audit,
                    emit.clone(),
                );
                Self {
                    agent: Some(Arc::new(agent)),
                    llm: Some(llm),
                    stt_provider,
                    whisper,
                    startup_error: None,
                }
            }
            Err(error) => {
                let message = format!("{error:#}");
                tracing::error!("configuration rejected: {message}");
                Self {
                    agent: None,
                    llm: None,
                    stt_provider: SttProvider::Model,
                    whisper: None,
                    startup_error: Some(message),
                }
            }
        }
    }
}

impl AppState {
    fn runtime(&self) -> std::sync::MutexGuard<'_, Runtime> {
        // A panic elsewhere must not turn every later command into an error.
        self.runtime
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn island_window(app: &AppHandle) -> Option<WebviewWindow> {
    app.get_webview_window("island")
}

/// Forward an agent event to the frontend and keep the window the right size.
fn forward(app: &AppHandle, event: AgentEvent) {
    if let AgentEvent::Phase { phase, .. } = &event {
        if let Some(window) = island_window(app) {
            island::apply_phase(&window, phase_name(*phase));
        }
    }

    let result = match event {
        AgentEvent::Phase { phase, label } => app.emit(
            "agent:phase",
            serde_json::json!({ "phase": phase, "label": label }),
        ),
        AgentEvent::Delta(text) => app.emit("agent:delta", serde_json::json!({ "text": text })),
        AgentEvent::Confirm(request) => app.emit("agent:confirm", request),
        AgentEvent::Final(text) => app.emit("agent:final", serde_json::json!({ "text": text })),
        AgentEvent::Error(message) => {
            app.emit("agent:error", serde_json::json!({ "message": message }))
        }
    };

    if let Err(error) = result {
        tracing::warn!("could not emit to the island: {error}");
    }
}

fn phase_name(phase: Phase) -> &'static str {
    match phase {
        Phase::Idle => "idle",
        Phase::Open => "open",
        Phase::Working => "working",
        Phase::Confirm => "confirm",
        Phase::Done => "done",
        Phase::Error => "error",
    }
}

// ------------------------------------------------------------------ commands

#[tauri::command]
async fn ask(
    state: State<'_, AppState>,
    prompt: String,
    voice: Option<bool>,
) -> Result<(), String> {
    let (agent, failure) = {
        let runtime = state.runtime();
        (runtime.agent.clone(), runtime.startup_error.clone())
    };
    if let Some(message) = failure {
        return Err(message);
    }
    let Some(agent) = agent else {
        return Err(NOT_CONFIGURED.to_string());
    };

    // Read by the event forwarder when the answer arrives. Cleared afterwards
    // so a failed turn cannot make the next one speak.
    state.voice.turn.store(voice.unwrap_or(false), Ordering::SeqCst);
    let result = agent
        .ask(&prompt)
        .await
        .map_err(|error| format!("{error:#}"));
    state.voice.turn.store(false, Ordering::SeqCst);
    result
}

#[tauri::command]
fn confirm(state: State<'_, AppState>, id: String, allow: bool) {
    if let Some(agent) = state.runtime().agent.clone() {
        agent.confirm(&id, allow);
    }
}

#[tauri::command]
fn cancel(state: State<'_, AppState>) {
    // Esc means stop, including the voice in the middle of a sentence.
    speech::stop();
    if let Some(agent) = state.runtime().agent.clone() {
        agent.cancel();
    }
}

#[tauri::command]
async fn start_dictation(state: State<'_, AppState>) -> Result<(), String> {
    // Half duplex: the microphone must not record Jarvis's own voice.
    speech::stop();

    let failure = state.runtime().startup_error.clone();
    if let Some(message) = failure {
        return Err(message);
    }

    let mut slot = state.recorder.lock().await;
    if slot.is_some() {
        return Ok(());
    }
    *slot = Some(
        Recorder::start()
            .await
            .map_err(|error| format!("{error:#}"))?,
    );
    Ok(())
}

/// Stop recording and return the transcript. Errors are shown in the island,
/// so the user learns why (no mic, a recogniser that cannot hear, silence).
#[tauri::command]
async fn stop_dictation(state: State<'_, AppState>) -> Result<String, String> {
    let recorder = state.recorder.lock().await.take();
    let Some(recorder) = recorder else {
        return Err("not recording".to_string());
    };

    let wav = recorder
        .stop()
        .await
        .map_err(|error| format!("{error:#}"))?;
    if wav.len() < 2_000 {
        return Err("that was too short to hear anything".to_string());
    }

    // Both paths return plain text: the text model answers the turn either way.
    let provider = state.runtime().stt_provider;
    let transcript = match provider {
        SttProvider::Windows => {
            jarvis_core::platform::transcribe::transcribe_wav(&wav).await
        }
        SttProvider::Whisper => {
            let Some((binary, model)) = state.runtime().whisper.clone() else {
                return Err(NOT_CONFIGURED.to_string());
            };
            jarvis_core::platform::transcribe::transcribe_with_whisper(
                &wav, &binary, &model,
            )
            .await
        }
        SttProvider::Model => {
            let llm = state.runtime().llm.clone();
            let Some(llm) = llm else {
                return Err(NOT_CONFIGURED.to_string());
            };
            llm.transcribe(&wav).await
        }
    };
    transcript.map_err(|error| format!("{error:#}"))
}

#[tauri::command]
fn ui_phase(app: AppHandle, phase: String) {
    if let Some(window) = island_window(&app) {
        island::apply_phase(&window, &phase);
    }
}

// ------------------------------------------------------------------ settings

/// What the panel may see. Never a key: only whether one is stored.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct SettingsView {
    base_url: String,
    model: String,
    max_steps: usize,
    stt_provider: String,
    whisper_bin: String,
    whisper_model: String,
    stt_model: String,
    stt_base_url: String,
    fallback_base_url: String,
    fallback_model: String,
    speak: bool,
    voice: String,
    speech_rate: i32,
    has_api_key: bool,
    has_stt_api_key: bool,
    /// Variables set in the environment or `.env`; those fields are read-only.
    env_locked: Vec<&'static str>,
    /// Why the current configuration was rejected, if it was.
    error: Option<String>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SettingsDraft {
    #[serde(default)]
    base_url: String,
    #[serde(default)]
    model: String,
    #[serde(default = "default_steps")]
    max_steps: u32,
    #[serde(default = "default_stt_provider")]
    stt_provider: String,
    #[serde(default)]
    whisper_bin: String,
    #[serde(default)]
    whisper_model: String,
    #[serde(default)]
    stt_model: String,
    #[serde(default)]
    stt_base_url: String,
    #[serde(default)]
    fallback_base_url: String,
    #[serde(default)]
    fallback_model: String,
    /// Only sent when the user typed a new one: the panel never receives the
    /// stored key, so a blank field means "keep it" rather than "erase it".
    #[serde(default)]
    api_key: String,
    #[serde(default)]
    stt_api_key: String,
    /// Explicit removal, since a blank field cannot express it.
    #[serde(default)]
    clear_api_key: bool,
    #[serde(default)]
    clear_stt_api_key: bool,
    #[serde(default = "default_speak")]
    speak: bool,
    #[serde(default)]
    voice: String,
    #[serde(default)]
    speech_rate: i32,
}

fn default_speak() -> bool {
    true
}

fn default_stt_provider() -> String {
    "model".to_string()
}

/// Result of the panel's "test" button: no tokens are spent, so it is safe to
/// press repeatedly.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ConnectionReport {
    latency_ms: u64,
    models: Vec<String>,
}

fn default_steps() -> u32 {
    8
}

/// Trimmed value, with blank treated as "not set".
fn non_empty(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn settings_view(error: Option<String>) -> Result<SettingsView, String> {
    let settings = config::load_settings().map_err(|error| format!("{error:#}"))?;
    let resolved = config::resolve(&settings);

    Ok(SettingsView {
        base_url: resolved.base_url,
        model: resolved.model,
        max_steps: resolved.max_steps,
        stt_provider: resolved.stt_provider.as_str().to_string(),
        whisper_bin: resolved.whisper_bin.unwrap_or_default(),
        whisper_model: resolved.whisper_model.unwrap_or_default(),
        stt_model: resolved.stt_model.unwrap_or_default(),
        stt_base_url: resolved.stt_base_url.unwrap_or_default(),
        fallback_base_url: resolved.fallback_base_url.unwrap_or_default(),
        fallback_model: resolved.fallback_model.unwrap_or_default(),
        speak: resolved.speak,
        voice: resolved.voice,
        speech_rate: resolved.speech_rate,
        has_api_key: !resolved.api_key.trim().is_empty(),
        has_stt_api_key: resolved
            .stt_api_key
            .as_deref()
            .is_some_and(|key| !key.trim().is_empty()),
        env_locked: config::env_locked_names(),
        error,
    })
}

/// Check the *draft* values, not the saved ones, so the panel can say whether a
/// configuration works before it is committed. A blank key means the stored one,
/// because the webview never receives a key to send back.
#[tauri::command]
async fn test_settings(draft: SettingsDraft) -> Result<ConnectionReport, String> {
    let stored = config::resolve(&config::load_settings().map_err(|error| format!("{error:#}"))?);

    let base_url = non_empty(&draft.base_url).unwrap_or(stored.base_url);
    let key = non_empty(&draft.api_key).unwrap_or(stored.api_key);
    let model = non_empty(&draft.model).unwrap_or(stored.model);

    let workspace = config::jarvis_dir()
        .map_err(|error| format!("{error:#}"))?
        .join("workspace");
    let llm = LlmClient::new(&Config::new(base_url, key, model, workspace));

    let started = std::time::Instant::now();
    let models = llm.list_models().await.map_err(|error| format!("{error:#}"))?;

    Ok(ConnectionReport {
        latency_ms: started.elapsed().as_millis() as u64,
        models,
    })
}

#[tauri::command]
fn get_settings(state: State<'_, AppState>) -> Result<SettingsView, String> {
    settings_view(state.runtime().startup_error.clone())
}

/// Write the file and rebuild the runtime. A rejected configuration is reported
/// in the returned view instead of as a command error, so the panel stays open
/// with the user's values still in front of them.
#[tauri::command]
fn save_settings(state: State<'_, AppState>, draft: SettingsDraft) -> Result<SettingsView, String> {
    let mut settings = config::load_settings().map_err(|error| format!("{error:#}"))?;

    settings.base_url = non_empty(&draft.base_url);
    settings.model = non_empty(&draft.model);
    settings.max_steps = Some(draft.max_steps.clamp(1, 24) as usize);
    settings.stt_provider = non_empty(&draft.stt_provider);
    settings.whisper_bin = non_empty(&draft.whisper_bin);
    settings.whisper_model = non_empty(&draft.whisper_model);
    settings.stt_model = non_empty(&draft.stt_model);
    settings.stt_base_url = non_empty(&draft.stt_base_url);
    settings.fallback_base_url = non_empty(&draft.fallback_base_url);
    settings.fallback_model = non_empty(&draft.fallback_model);
    settings.speak = Some(draft.speak);
    settings.voice = non_empty(&draft.voice);
    settings.speech_rate = Some(draft.speech_rate.clamp(-10, 10));
    if let Some(key) = non_empty(&draft.api_key) {
        settings.api_key = Some(key);
    }
    if let Some(key) = non_empty(&draft.stt_api_key) {
        settings.stt_api_key = Some(key);
    }
    // Applied after the assignments above, so an explicit "forget" wins.
    if draft.clear_api_key {
        settings.api_key = None;
    }
    if draft.clear_stt_api_key {
        settings.stt_api_key = None;
    }

    config::save_settings(&settings).map_err(|error| format!("{error:#}"))?;

    let mut runtime = state.runtime();
    *runtime = Runtime::build(&state.emit, &state.voice);
    let error = runtime.startup_error.clone();
    drop(runtime);

    settings_view(error)
}

// --------------------------------------------------------------------- setup

pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    tauri::Builder::default()
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, shortcut, event| {
                    if event.state() != ShortcutState::Pressed {
                        return;
                    }

                    let toggle = Shortcut::new(Some(Modifiers::CONTROL), Code::Space);
                    if shortcut == &toggle {
                        if let Some(window) = island_window(app) {
                            island::apply_phase(&window, "open");
                            let _ = window.show();
                            let _ = window.set_focus();
                            let _ = app.emit("agent:focus-input", ());
                        }
                        return;
                    }

                    let dictate =
                        Shortcut::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::Space);
                    if shortcut == &dictate {
                        if let Some(window) = island_window(app) {
                            island::apply_phase(&window, "open");
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                        let _ = app.emit("agent:dictate", ());
                        return;
                    }

                    let quit =
                        Shortcut::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::KeyQ);
                    if shortcut == &quit {
                        app.exit(0);
                    }
                })
                .build(),
        )
        .setup(|app| {
            let handle = app.handle().clone();

            let voice = VoiceState::default();
            let emit: Emitter = {
                let handle = handle.clone();
                let voice = voice.clone();
                Arc::new(move |event: AgentEvent| {
                    // Captured before the event is consumed; spoken after the
                    // island has been told, so the screen is never behind the voice.
                    let answer = match &event {
                        AgentEvent::Final(text) => Some(text.clone()),
                        _ => None,
                    };
                    forward(&handle, event);
                    if let Some(text) = answer {
                        voice.answer(&text);
                    }
                })
            };

            let runtime = Runtime::build(&emit, &voice);
            let failure = runtime.startup_error.clone();

            app.manage(AppState {
                runtime: std::sync::Mutex::new(runtime),
                recorder: tokio::sync::Mutex::new(None),
                emit,
                voice,
            });

            if let Some(window) = island_window(&handle) {
                island::park(&window);
                island::spawn_passthrough_loop(window.clone());
                let _ = window.show();
            }

            if let Some(message) = failure {
                // Surface it in the island rather than failing silently.
                let _ = handle.emit("agent:error", serde_json::json!({ "message": message }));
            }

            // Ctrl+Space to summon, Ctrl+Shift+Space to dictate, Ctrl+Shift+Q to quit.
            let toggle = Shortcut::new(Some(Modifiers::CONTROL), Code::Space);
            let dictate = Shortcut::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::Space);
            let quit = Shortcut::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::KeyQ);
            for shortcut in [toggle, dictate, quit] {
                if let Err(error) = handle.global_shortcut().register(shortcut) {
                    tracing::warn!("could not register global shortcut: {error}");
                }
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            ask,
            confirm,
            cancel,
            ui_phase,
            start_dictation,
            stop_dictation,
            get_settings,
            save_settings,
            test_settings
        ])
        .build(tauri::generate_context!())
        .expect("Jarvis failed to start")
        .run(|_, event| {
            // A child process would otherwise finish the sentence after a quit.
            if let tauri::RunEvent::Exit = event {
                speech::stop();
            }
        });
}
