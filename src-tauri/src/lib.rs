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

struct AppState {
    agent: Option<Arc<Agent>>,
    /// Set when startup failed, e.g. a missing API key.
    startup_error: Option<String>,
    /// Shares the agent's endpoint and key; used only for voice transcription.
    llm: Option<Arc<LlmClient>>,
    /// The microphone while a dictation is in progress.
    recorder: tokio::sync::Mutex<Option<Recorder>>,
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
    if let Some(message) = &state.startup_error {
        return Err(message.clone());
    }
    let Some(agent) = state.agent.clone() else {
        return Err("Jarvis is not configured yet. See .env.example.".to_string());
    };

    agent
        .ask(&prompt)
        .await
        .map_err(|error| format!("{error:#}"))
}

#[tauri::command]
fn confirm(state: State<'_, AppState>, id: String, allow: bool) {
    if let Some(agent) = &state.agent {
        agent.confirm(&id, allow);
    }
}

#[tauri::command]
fn cancel(state: State<'_, AppState>) {
    if let Some(agent) = &state.agent {
        agent.cancel();
    }
}

#[tauri::command]
async fn start_dictation(state: State<'_, AppState>) -> Result<(), String> {
    if let Some(message) = &state.startup_error {
        return Err(message.clone());
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

    let Some(llm) = state.llm.clone() else {
        return Err("Jarvis is not configured yet. See .env.example.".to_string());
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

            let (agent, llm, startup_error) = match Config::load() {
                Ok(config) => {
                    tracing::info!("starting with {}", config.describe());
                    let audit = config::audit_log_path()
                        .map(Audit::new)
                        .unwrap_or_else(|_| Audit::disabled());
                    let llm = Arc::new(LlmClient::new(&config));
                    let agent =
                        Agent::from_config(config, platform::current(), audit, emit.clone());
                    (Some(Arc::new(agent)), Some(llm), None)
                }
                Err(error) => {
                    let message = format!("{error:#}");
                    tracing::error!("startup failed: {message}");
                    (None, None, Some(message))
                }
            };

            let failure = startup_error.clone();
            app.manage(AppState {
                agent,
                llm,
                startup_error,
                recorder: tokio::sync::Mutex::new(None),
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
            stop_dictation
        ])
        .run(tauri::generate_context!())
        .expect("Jarvis failed to start");
}
