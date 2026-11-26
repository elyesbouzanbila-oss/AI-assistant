import { useCallback, useEffect, useRef, useState } from "react";
import type { ReactNode } from "react";
import {
  ask,
  cancel,
  confirm,
  getSettings,
  isLive,
  saveSettings,
  testSettings,
  setPhase as pushPhase,
  startDictation,
  stopDictation,
  subscribe,
} from "./lib/bridge";
import type {
  BridgeEvent,
  ChatLine,
  ConfirmRequest,
  Phase,
  SettingsDraft,
  SettingsView,
} from "./lib/types";

const COLLAPSE_MS = 2600;
/** A resize makes the webview emit leave/enter events that are not real hover changes. */
const HOVER_SETTLE_MS = 220;
/** Absorbs a single synthetic leave without making the island flicker. */
const CLOSE_DELAY_MS = 120;
/** Matches `MAX_SECONDS` in crates/jarvis-core/src/platform/recorder.rs. */
const DICTATION_MAX_MS = 30_000;

const PRESETS = [
  {
    name: "OpenRouter",
    baseUrl: "https://openrouter.ai/api/v1",
    model: "google/gemini-2.5-flash",
  },
  {
    name: "OpenAI",
    baseUrl: "https://api.openai.com/v1",
    model: "gpt-4o-mini",
  },
  {
    name: "Ollama",
    baseUrl: "http://localhost:11434/v1",
    model: "minicpm5:latest",
  },
  { name: "Z.ai", baseUrl: "https://api.z.ai/api/paas/v4", model: "glm-4.6v" },
];

const SUGGESTIONS = [
  "Open Notepad",
  "What's on my screen?",
  "Close my browser",
];

const STATUS: Record<Phase, string> = {
  idle: "",
  open: "Ready",
  settings: "Settings",
  working: "Working",
  confirm: "Needs your OK",
  done: "Done",
  error: "Blocked",
};

export function Island() {
  const [phase, setPhase] = useState<Phase>("idle");
  const [label, setLabel] = useState("Thinking");
  const [lines, setLines] = useState<ChatLine[]>([]);
  const [pending, setPending] = useState<ConfirmRequest | null>(null);
  const [draft, setDraft] = useState("");
  const [notice, setNotice] = useState("");
  const [dictating, setDictating] = useState(false);
  const [voiceNote, setVoiceNote] = useState("");
  const [transcribing, setTranscribing] = useState(false);
  const [settings, setSettings] = useState<SettingsView | null>(null);
  const [form, setForm] = useState<SettingsDraft | null>(null);
  const [settingsNote, setSettingsNote] = useState("");
  const [saving, setSaving] = useState(false);
  const [testing, setTesting] = useState(false);
  const [probe, setProbe] = useState<{ ok: boolean; text: string } | null>(
    null,
  );
  const [models, setModels] = useState<string[]>([]);

  const inputRef = useRef<HTMLInputElement>(null);
  const logRef = useRef<HTMLDivElement>(null);
  const streamingId = useRef<string | null>(null);
  const collapseTimer = useRef<number | null>(null);
  // A ref as well as state: the shortcut and timer callbacks read it out of
  // date with React renders.
  const dictatingRef = useRef(false);
  /** The transcript dictation produced, while the composer still matches it. */
  const dictatedRef = useRef<string | null>(null);
  /** Mirrors `draft` so an async callback reads the current text, not a stale render. */
  const draftRef = useRef("")
  const closeTimer = useRef<number | null>(null);
  const openTimer = useRef<number | null>(null);
  const pointerInside = useRef(false);
  /** Hover events are unreliable for a moment after a window resize. */
  const hoverGuard = useRef(0);

  const clearClose = useCallback(() => {
    if (closeTimer.current !== null) {
      window.clearTimeout(closeTimer.current);
      closeTimer.current = null;
    }
    if (openTimer.current !== null) {
      window.clearTimeout(openTimer.current);
      openTimer.current = null;
    }
  }, []);

  const clearCollapse = () => {
    if (collapseTimer.current !== null) {
      window.clearTimeout(collapseTimer.current);
      collapseTimer.current = null;
    }
  };

  const collapse = useCallback(() => {
    clearCollapse();
    streamingId.current = null;
    setPending(null);
    setPhase("idle");
  }, []);

  const closeSettings = useCallback(() => {
    setForm(null);
    setSettings(null);
    setSettingsNote("");
    void pushPhase("open");
    setPhase("open");
  }, []);

  const openSettings = async () => {
    clearCollapse();
    setSettingsNote("");
    setProbe(null);
    setPhase("settings");
    void pushPhase("settings");
    try {
      const view = await getSettings();
      setSettings(view);
      setForm({
        baseUrl: view.baseUrl,
        model: view.model,
        maxSteps: view.maxSteps,
        sttProvider: view.sttProvider,
        whisperBin: view.whisperBin,
        whisperModel: view.whisperModel,
        sttModel: view.sttModel,
        sttBaseUrl: view.sttBaseUrl,
        fallbackBaseUrl: view.fallbackBaseUrl,
        fallbackModel: view.fallbackModel,
        speak: view.speak,
        voice: view.voice,
        speechRate: view.speechRate,
        apiKey: "",
        sttApiKey: "",
        clearApiKey: false,
        clearSttApiKey: false,
      });
    } catch (error) {
      setSettingsNote(String(error));
    }
  };

  const setRecording = (on: boolean) => {
    dictatingRef.current = on;
    setDictating(on);
  };

  const writeDraft = (value: string) => {
    draftRef.current = value;
    setDraft(value);
  };

  /** Hand a prompt to the agent. `byVoice` makes the answer spoken. */
  const sendTurn = (text: string, byVoice: boolean) => {
    const prompt = text.trim();
    if (!prompt) return;
    dictatedRef.current = null;
    setLines((prev) => [
      ...prev,
      { id: `u-${Date.now()}`, role: "user", text: prompt },
    ]);
    writeDraft("");
    setNotice("");
    streamingId.current = null;
    void ask(prompt, byVoice);
  };

  const beginDictation = useCallback(async () => {
    if (dictatingRef.current) return;
    setVoiceNote("");
    try {
      await startDictation();
      setRecording(true);
    } catch (error) {
      setVoiceNote(String(error));
    }
  }, []);

  /** Stop recording. `keep` false drops the audio, e.g. when Esc cancels. */
  const finishDictation = useCallback(async (keep: boolean) => {
    if (!dictatingRef.current) return;
    setRecording(false);
    // The WAV is transcribed on stop, so name what the wait is for.
    setTranscribing(true);
    try {
      const text = (await stopDictation()).trim();
      if (!keep || !text) return;
      // An empty composer makes dictation one action: stopping the mic sends
      // the transcript, and the answer is spoken because the turn began by
      // voice. A half-typed draft is never sent by surprise: the transcript
      // is appended and waits for Send.
      if (!draftRef.current.trim()) {
        sendTurn(text, true);
        return;
      }
      dictatedRef.current = null;
      writeDraft(`${draftRef.current.trim()} ${text}`);
      window.requestAnimationFrame(() => inputRef.current?.focus());
    } catch (error) {
      setVoiceNote(String(error));
    } finally {
      setTranscribing(false);
    }
  }, []);

  const toggleDictation = () => {
    if (dictatingRef.current) void finishDictation(true);
    else void beginDictation();
  };

  const handleEvent = useCallback(
    (event: BridgeEvent) => {
      switch (event.type) {
        case "phase":
          clearCollapse();
          clearClose();
          hoverGuard.current = Date.now() + HOVER_SETTLE_MS;
          setPhase(event.phase);
          if (event.label) setLabel(event.label);
          break;

        case "delta": {
          const id = streamingId.current;
          if (id) {
            setLines((prev) =>
              prev.map((line) =>
                line.id === id ? { ...line, text: line.text + event.text } : line,
              ),
            );
          } else {
            const newId = `a-${Date.now()}`;
            streamingId.current = newId;
            setLines((prev) => [
              ...prev,
              { id: newId, role: "assistant", text: event.text },
            ]);
          }
          break;
        }

        case "confirm":
          streamingId.current = null;
          setPending(event.request);
          setPhase("confirm");
          break;

        case "final":
          streamingId.current = null;
          setPending(null);
          setNotice(event.text);
          setPhase("done");
          clearCollapse();
          collapseTimer.current = window.setTimeout(collapse, COLLAPSE_MS);
          break;

        case "error":
          streamingId.current = null;
          setPending(null);
          setNotice(event.message);
          setPhase("error");
          break;

        case "focus":
          setPhase((current) => (current === "idle" ? "open" : current));
          break;

        case "dictate":
          // Functional update: this listener is registered once per render.
          setPhase((current) =>
            current === "working" || current === "confirm" ? current : "open",
          );
          if (dictatingRef.current) void finishDictation(true);
          else void beginDictation();
          break;
      }
    },
    [collapse, clearClose, beginDictation, finishDictation],
  );

  useEffect(() => {
    document.body.className = isLive ? "live" : "demo";
    return subscribe(handleEvent);
  }, [handleEvent]);

  useEffect(() => clearClose, [clearClose]);

  useEffect(() => {
    if (phase === "open") {
      inputRef.current?.focus();
    }
  }, [phase]);

  useEffect(() => {
    logRef.current?.scrollTo({ top: logRef.current.scrollHeight });
  }, [lines]);

  // Stop by itself rather than hold the microphone open indefinitely.
  useEffect(() => {
    if (!dictating) return;
    const timer = window.setTimeout(
      () => void finishDictation(true),
      DICTATION_MAX_MS,
    );
    return () => window.clearTimeout(timer);
  }, [dictating, finishDictation]);

  // Leaving the island drops whatever was being recorded.
  useEffect(() => {
    if (phase === "idle" && dictatingRef.current) void finishDictation(false);
  }, [phase, finishDictation]);

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      event.preventDefault();
      if (phase === "settings") {
        closeSettings();
        return;
      }
      if (dictatingRef.current) void finishDictation(false);
      void cancel();
      collapse();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [closeSettings, collapse, finishDictation, phase]);

  const open = () => {
    clearCollapse();
    clearClose();
    hoverGuard.current = Date.now() + HOVER_SETTLE_MS;
    setPhase("open");
    void pushPhase("open");
  };

  const close = () => {
    clearClose();
    hoverGuard.current = Date.now() + HOVER_SETTLE_MS;
    void pushPhase("idle");
    collapse();
  };

  const submit = () => {
    const text = draft.trim();
    if (!text) return;
    const byVoice = dictatedRef.current !== null && dictatedRef.current === text;
    sendTurn(text, byVoice);
  };

  const edit = (patch: Partial<SettingsDraft>) => {
    setForm((prev) => (prev ? { ...prev, ...patch } : prev));
    setSettings((prev) => (prev?.error ? { ...prev, error: null } : prev));
    setSettingsNote("");
    setProbe(null);
  };

  const runTest = async () => {
    if (!form || testing) return;
    setTesting(true);
    setProbe(null);
    try {
      const report = await testSettings(form);
      setModels(report.models);
      setProbe({
        ok: true,
        text: `reachable in ${report.latencyMs} ms, ${report.models.length} models`,
      });
    } catch (error) {
      setProbe({ ok: false, text: String(error) });
    } finally {
      setTesting(false);
    }
  };

  const forgetKey = (which: "api" | "stt") =>
    edit(
      which === "api"
        ? { apiKey: "", clearApiKey: true }
        : { sttApiKey: "", clearSttApiKey: true },
    );

  const submitSettings = async () => {
    if (!form || saving) return;
    setSaving(true);
    try {
      const view = await saveSettings(form);
      setSettings(view);
      setForm({
        ...form,
        apiKey: "",
        sttApiKey: "",
        clearApiKey: false,
        clearSttApiKey: false,
      });
      setSettingsNote(view.error ? "" : "Saved. The new settings are live.");
    } catch (error) {
      setSettingsNote(String(error));
    } finally {
      setSaving(false);
    }
  };

  const envLocked = (name: string) => settings?.envLocked.includes(name) ?? false;

  // Save stays disabled until something actually differs, so a stray click
  // cannot rewrite the file with the same values.
  const dirty =
    !!settings &&
    !!form &&
    (form.apiKey.trim() !== "" ||
      form.sttApiKey.trim() !== "" ||
      form.clearApiKey ||
      form.clearSttApiKey ||
      form.baseUrl !== settings.baseUrl ||
      form.model !== settings.model ||
      form.maxSteps !== settings.maxSteps ||
      form.sttProvider !== settings.sttProvider ||
      form.whisperBin !== settings.whisperBin ||
      form.whisperModel !== settings.whisperModel ||
      form.sttModel !== settings.sttModel ||
      form.sttBaseUrl !== settings.sttBaseUrl ||
      form.fallbackBaseUrl !== settings.fallbackBaseUrl ||
      form.fallbackModel !== settings.fallbackModel ||
      form.speak !== settings.speak ||
      form.voice !== settings.voice ||
      form.speechRate !== settings.speechRate);

  const decide = (allow: boolean) => {
    if (!pending) return;
    void confirm(pending.id, allow);
    setPending(null);
    setLabel(allow ? "Working" : "Cancelling");
    setPhase("working");
  };

  const status =
    phase === "working" ? label : phase === "done" ? notice : STATUS[phase];

  return (
    <>
      <div
        className="island"
        data-phase={phase}
        onMouseEnter={() => {
          pointerInside.current = true;
          clearClose();
          if (phase !== "idle") return;
          const wait = hoverGuard.current - Date.now();
          if (wait <= 0) {
            open();
          } else {
            // The pointer may still be here when the resize settles.
            openTimer.current = window.setTimeout(() => {
              openTimer.current = null;
              if (pointerInside.current) open();
            }, wait);
          }
        }}
        onMouseLeave={() => {
          pointerInside.current = false;
          if (openTimer.current !== null) {
            window.clearTimeout(openTimer.current);
            openTimer.current = null;
          }
          if (
            phase !== "open" ||
            draft.trim() ||
            Date.now() < hoverGuard.current
          ) {
            return;
          }
          closeTimer.current = window.setTimeout(() => {
            closeTimer.current = null;
            close();
          }, CLOSE_DELAY_MS);
        }}
      >
        <header className="bar" onClick={() => phase === "idle" && open()}>
          <div className="orb" data-state={phase} />
          <div className="title">
            <span className="name">Jarvis</span>
            {phase !== "idle" && <span className="sub">{status}</span>}
          </div>
          {phase === "idle" && <span className="hint">Ctrl+Space</span>}
          {phase === "open" && (
            <div className="bar-actions">
              <button
                className="icon"
                onClick={() => void openSettings()}
                aria-label="Settings"
                title="Settings"
              >
                <GearIcon />
              </button>
              <button className="icon" onClick={close} aria-label="Close">
                &times;
              </button>
            </div>
          )}
          {phase === "settings" && (
            <div className="bar-actions">
              <button
                className="icon"
                onClick={closeSettings}
                aria-label="Back to chat"
                title="Back to chat"
              >
                &times;
              </button>
            </div>
          )}
        </header>

        <div className="body">
          {phase === "settings" && (
            <form
              className="settings"
              onSubmit={(event) => {
                event.preventDefault();
                void submitSettings();
              }}
            >
              <div className="presets">
                <span className="presets-label">Presets</span>
                {PRESETS.map((preset) => (
                  <button
                    key={preset.name}
                    type="button"
                    onClick={() =>
                      edit({ baseUrl: preset.baseUrl, model: preset.model })
                    }
                  >
                    {preset.name}
                  </button>
                ))}
              </div>

              <Field label="Endpoint" locked={envLocked("JARVIS_BASE_URL")}>
                <input
                  value={form?.baseUrl ?? ""}
                  onChange={(event) => edit({ baseUrl: event.target.value })}
                  placeholder="https://openrouter.ai/api/v1"
                  spellCheck={false}
                  autoComplete="off"
                  disabled={envLocked("JARVIS_BASE_URL")}
                />
              </Field>

              <div className="field-row">
                <Field label="Model" locked={envLocked("JARVIS_MODEL")}>
                  <input
                    value={form?.model ?? ""}
                    onChange={(event) => edit({ model: event.target.value })}
                    placeholder="google/gemini-2.5-flash"
                    list="jarvis-models"
                    spellCheck={false}
                    autoComplete="off"
                    disabled={envLocked("JARVIS_MODEL")}
                  />
                </Field>
                <Field label="Steps" locked={envLocked("JARVIS_MAX_STEPS")}>
                  <input
                    type="number"
                    min={1}
                    max={24}
                    value={form?.maxSteps ?? 8}
                    onChange={(event) =>
                      edit({
                        maxSteps: Math.min(
                          24,
                          Math.max(1, Number(event.target.value) || 1),
                        ),
                      })
                    }
                    disabled={envLocked("JARVIS_MAX_STEPS")}
                  />
                </Field>
              </div>

              <Field label="API key" locked={envLocked("JARVIS_API_KEY")}>
                <div className="field-with-action">
                  <input
                    type="password"
                    value={form?.apiKey ?? ""}
                    onChange={(event) =>
                      edit({ apiKey: event.target.value, clearApiKey: false })
                    }
                    placeholder={
                      settings?.hasApiKey && !form?.clearApiKey
                        ? "saved - type to replace it"
                        : form?.clearApiKey
                          ? "saved key will be removed"
                          : "sk-or-v1-..."
                    }
                    autoComplete="off"
                    spellCheck={false}
                    disabled={envLocked("JARVIS_API_KEY")}
                  />
                  {settings?.hasApiKey && !form?.clearApiKey && (
                    <button
                      type="button"
                      className="field-action"
                      onClick={() => forgetKey("api")}
                      title="Remove the saved key when you save"
                    >
                      Forget
                    </button>
                  )}
                </div>
              </Field>

              <div className="settings-section">
                Voice <span>optional</span>
              </div>

              <Field
                label="Transcription"
                locked={envLocked("JARVIS_STT_PROVIDER")}
              >
                <select
                  value={form?.sttProvider ?? "model"}
                  onChange={(event) =>
                    edit({ sttProvider: event.target.value })
                  }
                  disabled={envLocked("JARVIS_STT_PROVIDER")}
                >
                  <option value="model">Voice model (audio input)</option>
                  <option value="windows">Windows speech recognition</option>
                  <option value="whisper">Whisper in WSL (local)</option>
                </select>
              </Field>

              <div className="field-row voice">
                <Field label="Whisper command" locked={envLocked("JARVIS_WHISPER_BIN")}>
                  <input
                    value={form?.whisperBin ?? ""}
                    onChange={(event) => edit({ whisperBin: event.target.value })}
                    placeholder="whisper-cli"
                    spellCheck={false}
                    autoComplete="off"
                    disabled={envLocked("JARVIS_WHISPER_BIN")}
                  />
                </Field>
                <Field label="Whisper model" locked={envLocked("JARVIS_WHISPER_MODEL")}>
                  <input
                    value={form?.whisperModel ?? ""}
                    onChange={(event) => edit({ whisperModel: event.target.value })}
                    placeholder="Linux path inside WSL"
                    spellCheck={false}
                    autoComplete="off"
                    disabled={envLocked("JARVIS_WHISPER_MODEL")}
                  />
                </Field>
              </div>

              <div className="field-row voice">
                <Field label="Voice model" locked={envLocked("JARVIS_STT_MODEL")}>
                  <input
                    value={form?.sttModel ?? ""}
                    onChange={(event) => edit({ sttModel: event.target.value })}
                    placeholder="same as the model above"
                    spellCheck={false}
                    autoComplete="off"
                    disabled={envLocked("JARVIS_STT_MODEL")}
                  />
                </Field>
                <Field
                  label="Voice endpoint"
                  locked={envLocked("JARVIS_STT_BASE_URL")}
                >
                  <input
                    value={form?.sttBaseUrl ?? ""}
                    onChange={(event) => edit({ sttBaseUrl: event.target.value })}
                    placeholder="same as the endpoint above"
                    spellCheck={false}
                    autoComplete="off"
                    disabled={envLocked("JARVIS_STT_BASE_URL")}
                  />
                </Field>
              </div>

              <Field label="Voice key" locked={envLocked("JARVIS_STT_API_KEY")}>
                <div className="field-with-action">
                  <input
                    type="password"
                    value={form?.sttApiKey ?? ""}
                    onChange={(event) =>
                      edit({ sttApiKey: event.target.value, clearSttApiKey: false })
                    }
                    placeholder={
                      settings?.hasSttApiKey && !form?.clearSttApiKey
                        ? "saved - type to replace it"
                        : form?.clearSttApiKey
                          ? "saved key will be removed"
                          : "only needed for a separate voice endpoint"
                    }
                    autoComplete="off"
                    spellCheck={false}
                    disabled={envLocked("JARVIS_STT_API_KEY")}
                  />
                  {settings?.hasSttApiKey && !form?.clearSttApiKey && (
                    <button
                      type="button"
                      className="field-action"
                      onClick={() => forgetKey("stt")}
                      title="Remove the saved voice key when you save"
                    >
                      Forget
                    </button>
                  )}
                </div>
              </Field>

              <div className="field-row voice">
                <Field label="Voice name" locked={envLocked("JARVIS_VOICE")}>
                  <input
                    value={form?.voice ?? ""}
                    onChange={(event) => edit({ voice: event.target.value })}
                    placeholder="system default"
                    spellCheck={false}
                    autoComplete="off"
                    disabled={envLocked("JARVIS_VOICE")}
                  />
                </Field>
                <Field
                  label="Speech rate"
                  locked={envLocked("JARVIS_SPEECH_RATE")}
                >
                  <input
                    type="number"
                    min={-10}
                    max={10}
                    value={form?.speechRate ?? 0}
                    onChange={(event) =>
                      edit({
                        speechRate: Math.min(
                          10,
                          Math.max(-10, Number(event.target.value) || 0),
                        ),
                      })
                    }
                    disabled={envLocked("JARVIS_SPEECH_RATE")}
                  />
                </Field>
              </div>

              <label className="field-check">
                <input
                  type="checkbox"
                  checked={form?.speak ?? true}
                  onChange={(event) => edit({ speak: event.target.checked })}
                  disabled={envLocked("JARVIS_SPEAK")}
                />
                <span>
                  Speak the answer <em>only when you dictated the request</em>
                </span>
              </label>

              <div className="settings-section">
                Fallback <span>local, started on demand</span>
              </div>

              <div className="field-row voice">
                <Field
                  label="Fallback endpoint"
                  locked={envLocked("JARVIS_FALLBACK_BASE_URL")}
                >
                  <input
                    value={form?.fallbackBaseUrl ?? ""}
                    onChange={(event) =>
                      edit({ fallbackBaseUrl: event.target.value })
                    }
                    placeholder="http://localhost:11434/v1"
                    spellCheck={false}
                    autoComplete="off"
                    disabled={envLocked("JARVIS_FALLBACK_BASE_URL")}
                  />
                </Field>
                <Field
                  label="Fallback model"
                  locked={envLocked("JARVIS_FALLBACK_MODEL")}
                >
                  <input
                    value={form?.fallbackModel ?? ""}
                    onChange={(event) => edit({ fallbackModel: event.target.value })}
                    placeholder="minicpm5:latest"
                    spellCheck={false}
                    autoComplete="off"
                    disabled={envLocked("JARVIS_FALLBACK_MODEL")}
                  />
                </Field>
              </div>

              <p className="field-note">
                Tried when the model above fails. WSL and Ollama start on their
                own; leave either field blank to turn the fallback off.
              </p>

              <datalist id="jarvis-models">
                {models.map((model) => (
                  <option key={model} value={model} />
                ))}
              </datalist>

              <div className="settings-foot">
                <button
                  type="button"
                  className="deny"
                  onClick={() => void runTest()}
                  disabled={testing || !form}
                  title="Check the endpoint and key without spending tokens"
                >
                  {testing ? "Testing..." : "Test"}
                </button>
                <span
                  className="settings-note"
                  data-tone={
                    probe
                      ? probe.ok
                        ? "ok"
                        : "bad"
                      : settings?.error
                        ? "bad"
                        : settingsNote
                          ? "ok"
                          : undefined
                  }
                >
                  {probe?.text ?? settings?.error ?? settingsNote}
                </span>
                <button
                  type="submit"
                  className="allow"
                  disabled={saving || !form || !dirty}
                >
                  {saving ? "Saving..." : "Save"}
                </button>
              </div>
            </form>
          )}

          {phase === "open" && (
            <>
              <div className="log" ref={logRef}>
                {lines.length === 0 ? (
                  <div className="empty">
                    <p className="empty-lead">
                      Ask, or pick something below.
                    </p>
                    <div className="chips">
                      {SUGGESTIONS.map((s) => (
                        <button key={s} onClick={() => writeDraft(s)}>
                          {s}
                        </button>
                      ))}
                    </div>
                  </div>
                ) : (
                  lines.map((line) => (
                    <div key={line.id} className={`line ${line.role}`}>
                      {line.text}
                    </div>
                  ))
                )}
              </div>
              {(voiceNote || transcribing) && (
                <div className={`voice-note${transcribing ? " busy" : ""}`}>
                  {transcribing ? "Transcribing…" : voiceNote}
                </div>
              )}
              <form
                className="composer"
                onSubmit={(event) => {
                  event.preventDefault();
                  submit();
                }}
              >
                <button
                  type="button"
                  className={`mic${dictating ? " recording" : ""}`}
                  onClick={() => void toggleDictation()}
                  disabled={transcribing}
                  aria-label={dictating ? "Stop dictation" : "Start dictation"}
                  title={
                    dictating ? "Stop dictation" : "Dictate (Ctrl+Shift+Space)"
                  }
                >
                  {dictating ? <span className="mic-stop" /> : <MicIcon />}
                </button>
                <input
                  ref={inputRef}
                  value={draft}
                  onChange={(event) => {
                    // Editing the transcript makes it a typed turn.
                    dictatedRef.current = null;
                    writeDraft(event.target.value);
                  }}
                  placeholder={
                    dictating
                      ? "Listening… click the mic to stop"
                      : "Ask Jarvis anything…"
                  }
                  spellCheck={false}
                  autoComplete="off"
                />
                <button
                  type="submit"
                  className="send"
                  aria-label="Send"
                  disabled={transcribing}
                >
                  &#8593;
                </button>
              </form>
            </>
          )}

          {phase === "working" && (
            <div className="working">
              <div className="wave" aria-hidden="true">
                <span />
                <span />
                <span />
                <span />
                <span />
              </div>
              <span className="working-label">{label}&hellip;</span>
            </div>
          )}

          {phase === "confirm" && pending && (
            <div className="confirm">
              <div className="confirm-head">
                <span className="warn">&#9888;</span>
                <span className="confirm-tool">{pending.tool}</span>
              </div>
              <p className="confirm-summary">{pending.summary}</p>
              <div className="confirm-actions">
                <button className="allow" onClick={() => decide(true)}>
                  Allow
                </button>
                <button className="deny" onClick={() => decide(false)}>
                  Deny
                </button>
              </div>
              <div className="countdown" />
            </div>
          )}

          {phase === "done" && (
            <div className="done">
              <span className="tick">&#10003;</span>
              <span>{notice}</span>
            </div>
          )}

          {phase === "error" && (
            <div className="failure">
              <span>{notice}</span>
              <button className="dismiss" onClick={collapse}>
                Dismiss
              </button>
            </div>
          )}
        </div>
      </div>

      {!isLive && (
        <div className="demo-caption">
          Browser demo &middot; desktop control and voice run in the real app on
          your PC
        </div>
      )}
    </>
  );
}

function Field({
  label,
  locked,
  children,
}: {
  label: string;
  locked: boolean;
  children: ReactNode;
}) {
  return (
    <label className="field">
      <span className="field-label">
        {label}
        {locked && (
          <span
            className="field-lock"
            title="Set in .env, which wins. Remove it there to edit it here."
          >
            env
          </span>
        )}
      </span>
      {children}
    </label>
  );
}

function GearIcon() {
  return (
    <svg
      width="15"
      height="15"
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="2"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
    >
      <circle cx="12" cy="12" r="3" />
      <path d="M19.4 15a1.7 1.7 0 0 0 .34 1.87l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.7 1.7 0 0 0-1.87-.34 1.7 1.7 0 0 0-1 1.55V21a2 2 0 1 1-4 0v-.09a1.7 1.7 0 0 0-1.11-1.55 1.7 1.7 0 0 0-1.87.34l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06A1.7 1.7 0 0 0 4.6 15a1.7 1.7 0 0 0-1.55-1H3a2 2 0 1 1 0-4h.09A1.7 1.7 0 0 0 4.6 8.9a1.7 1.7 0 0 0-.34-1.87l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06A1.7 1.7 0 0 0 9 4.6h.09A1.7 1.7 0 0 0 10.1 3.05V3a2 2 0 1 1 4 0v.09a1.7 1.7 0 0 0 1 1.55 1.7 1.7 0 0 0 1.87-.34l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06A1.7 1.7 0 0 0 19.4 9v.09a1.7 1.7 0 0 0 1.55 1H21a2 2 0 1 1 0 4h-.09a1.7 1.7 0 0 0-1.51 1z" />
    </svg>
  );
}

function MicIcon() {
  return (
    <svg
      width="15"
      height="15"
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="2"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
    >
      <rect x="9" y="2" width="6" height="12" rx="3" />
      <path d="M5 10v1a7 7 0 0 0 14 0v-1" />
      <line x1="12" y1="19" x2="12" y2="22" />
    </svg>
  );
}
