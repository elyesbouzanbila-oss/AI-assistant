//! Tauri shell for Jarvis.
//!
//! All real work happens in `jarvis-core`; this file only wires that into a
//! window: commands in, events out, plus the global shortcut.

mod island;

use std::sync::Arc;

use jarvis_core::agent::{Agent, AgentEvent, Emitter, Phase};
use jarvis_core::audit::Audit;
use jarvis_core::config::{self, Config};
use jarvis_core::llm::LlmClient;
use jarvis_core::platform;
use jarvis_core::platform::recorder::Recorder;
use tauri::{AppHandle, Emitter as _, Manager, State, WebviewWindow};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};

const NOT_CONFIGURED: &str =
    "Jarvis has no model yet. Open Settings (the gear icon) and add an endpoint, model and key.";

struct AppState {
    /// Rebuilt by `save_settings`, so a new endpoint or key applies at once.
    runtime: std::sync::Mutex<Runtime>,
    /// The microphone while a dictation is in progress.
    recorder: tokio::sync::Mutex<Option<Recorder>>,
    /// Kept so a rebuilt agent still reports to the same island.
    emit: Emitter,
}

struct Runtime {
    agent: Option<Arc<Agent>>,
    /// Shares the agent's endpoint and key; used only for voice transcription.
    llm: Option<Arc<LlmClient>>,
    /// Set when the configuration was rejected, e.g. a missing API key.
    startup_error: Option<String>,
}

impl Runtime {
    fn build(emit: &Emitter) -> Self {
        match Config::load() {
            Ok(config) => {
                tracing::info!("starting with {}", config.describe());
                let audit = config::audit_log_path()
                    .map(Audit::new)
                    .unwrap_or_else(|_| Audit::disabled());
                let llm = Arc::new(LlmClient::new(&config));
                let agent = Agent::from_config(config, platform::current(), audit, emit.clone());
                Self {
                    agent: Some(Arc::new(agent)),
                    llm: Some(llm),
                    startup_error: None,
                }
            }
            Err(error) => {
                let message = format!("{error:#}");
                tracing::error!("configuration rejected: {message}");
                Self {
                    agent: None,
                    llm: None,
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
async fn ask(state: State<'_, AppState>, prompt: String) -> Result<(), String> {
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

    agent
        .ask(&prompt)
        .await
        .map_err(|error| format!("{error:#}"))
}

#[tauri::command]
fn confirm(state: State<'_, AppState>, id: String, allow: bool) {
    if let Some(agent) = state.runtime().agent.clone() {
        agent.confirm(&id, allow);
    }
}

#[tauri::command]
fn cancel(state: State<'_, AppState>) {
    if let Some(agent) = state.runtime().agent.clone() {
        agent.cancel();
    }
}

#[tauri::command]
async fn start_dictation(state: State<'_, AppState>) -> Result<(), String> {
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
/// so the user learns why (no mic, model without audio input, silence).
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

    let llm = state.runtime().llm.clone();
    let Some(llm) = llm else {
        return Err(NOT_CONFIGURED.to_string());
    };
    llm.transcribe(&wav)
        .await
        .map_err(|error| format!("{error:#}"))
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
    stt_model: String,
    stt_base_url: String,
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
    #[serde(default)]
    stt_model: String,
    #[serde(default)]
    stt_base_url: String,
    /// Only sent when the user typed a new one: the panel never receives the
    /// stored key, so a blank field means "keep it" rather than "erase it".
    #[serde(default)]
    api_key: String,
    #[serde(default)]
    stt_api_key: String,
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
        stt_model: resolved.stt_model.unwrap_or_default(),
        stt_base_url: resolved.stt_base_url.unwrap_or_default(),
        has_api_key: !resolved.api_key.trim().is_empty(),
        has_stt_api_key: resolved
            .stt_api_key
            .as_deref()
            .is_some_and(|key| !key.trim().is_empty()),
        env_locked: config::env_locked_names(),
        error,
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
    settings.stt_model = non_empty(&draft.stt_model);
    settings.stt_base_url = non_empty(&draft.stt_base_url);
    if let Some(key) = non_empty(&draft.api_key) {
        settings.api_key = Some(key);
    }
    if let Some(key) = non_empty(&draft.stt_api_key) {
        settings.stt_api_key = Some(key);
    }

    config::save_settings(&settings).map_err(|error| format!("{error:#}"))?;

    let mut runtime = state.runtime();
    *runtime = Runtime::build(&state.emit);
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

            let emit: Emitter = {
                let handle = handle.clone();
                Arc::new(move |event: AgentEvent| forward(&handle, event))
            };

            let runtime = Runtime::build(&emit);
            let failure = runtime.startup_error.clone();

            app.manage(AppState {
                runtime: std::sync::Mutex::new(runtime),
                recorder: tokio::sync::Mutex::new(None),
                emit,
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
            save_settings
        ])
        .run(tauri::generate_context!())
        .expect("Jarvis failed to start");
}
