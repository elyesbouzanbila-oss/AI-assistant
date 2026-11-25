import { useCallback, useEffect, useRef, useState } from "react";
import type { ReactNode } from "react";
import {
  ask,
  cancel,
  confirm,
  getSettings,
  isLive,
  saveSettings,
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
/** Matches `MAX_SECONDS` in crates/jarvis-core/src/platform/recorder.rs. */
const DICTATION_MAX_MS = 30_000;

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
  const [settings, setSettings] = useState<SettingsView | null>(null);
  const [form, setForm] = useState<SettingsDraft | null>(null);
  const [settingsNote, setSettingsNote] = useState("");
  const [saving, setSaving] = useState(false);

  const inputRef = useRef<HTMLInputElement>(null);
  const logRef = useRef<HTMLDivElement>(null);
  const streamingId = useRef<string | null>(null);
  const collapseTimer = useRef<number | null>(null);
  // A ref as well as state: the shortcut and timer callbacks read it out of
  // date with React renders.
  const dictatingRef = useRef(false);

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
    setPhase("settings");
    void pushPhase("settings");
    try {
      const view = await getSettings();
      setSettings(view);
      setForm({
        baseUrl: view.baseUrl,
        model: view.model,
        maxSteps: view.maxSteps,
        sttModel: view.sttModel,
        sttBaseUrl: view.sttBaseUrl,
        apiKey: "",
        sttApiKey: "",
      });
    } catch (error) {
      setSettingsNote(String(error));
    }
  };

  const setRecording = (on: boolean) => {
    dictatingRef.current = on;
    setDictating(on);
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
    try {
      const text = (await stopDictation()).trim();
      if (!keep || !text) return;
      setDraft((prev) => (prev.trim() ? `${prev.trim()} ${text}` : text));
      window.requestAnimationFrame(() => inputRef.current?.focus());
    } catch (error) {
      setVoiceNote(String(error));
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
    [collapse, beginDictation, finishDictation],
  );

  useEffect(() => {
    document.body.className = isLive ? "live" : "demo";
    return subscribe(handleEvent);
  }, [handleEvent]);

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
    setPhase("open");
    void pushPhase("open");
  };

  const close = () => {
    void pushPhase("idle");
    collapse();
  };

  const submit = () => {
    const text = draft.trim();
    if (!text) return;
    setLines((prev) => [
      ...prev,
      { id: `u-${Date.now()}`, role: "user", text },
    ]);
    setDraft("");
    setNotice("");
    streamingId.current = null;
    void ask(text);
  };

  const edit = (patch: Partial<SettingsDraft>) => {
    setForm((prev) => (prev ? { ...prev, ...patch } : prev));
    setSettings((prev) => (prev?.error ? { ...prev, error: null } : prev));
    setSettingsNote("");
  };

  const submitSettings = async () => {
    if (!form || saving) return;
    setSaving(true);
    try {
      const view = await saveSettings(form);
      setSettings(view);
      setForm({ ...form, apiKey: "", sttApiKey: "" });
      setSettingsNote(view.error ? "" : "Saved. The new settings are live.");
    } catch (error) {
      setSettingsNote(String(error));
    } finally {
      setSaving(false);
    }
  };

  const envLocked = (name: string) => settings?.envLocked.includes(name) ?? false;

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
          if (phase === "idle") open();
        }}
        onMouseLeave={() => {
          if (phase === "open" && !draft.trim()) close();
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
                <input
                  type="password"
                  value={form?.apiKey ?? ""}
                  onChange={(event) => edit({ apiKey: event.target.value })}
                  placeholder={
                    settings?.hasApiKey
                      ? "saved - type to replace it"
                      : "sk-or-v1-..."
                  }
                  autoComplete="off"
                  spellCheck={false}
                  disabled={envLocked("JARVIS_API_KEY")}
                />
              </Field>

              <div className="settings-section">
                Voice <span>optional</span>
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
                <input
                  type="password"
                  value={form?.sttApiKey ?? ""}
                  onChange={(event) => edit({ sttApiKey: event.target.value })}
                  placeholder={
                    settings?.hasSttApiKey
                      ? "saved - type to replace it"
                      : "only needed for a separate voice endpoint"
                  }
                  autoComplete="off"
                  spellCheck={false}
                  disabled={envLocked("JARVIS_STT_API_KEY")}
                />
              </Field>

              <div className="settings-foot">
                <span
                  className="settings-note"
                  data-tone={
                    settings?.error ? "bad" : settingsNote ? "ok" : undefined
                  }
                >
                  {settings?.error ?? settingsNote}
                </span>
                <button
                  type="submit"
                  className="allow"
                  disabled={saving || !form}
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
                        <button key={s} onClick={() => setDraft(s)}>
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
              {voiceNote && <div className="voice-note">{voiceNote}</div>}
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
                  onChange={(event) => setDraft(event.target.value)}
                  placeholder={
                    dictating
                      ? "Listening… click the mic to stop"
                      : "Ask Jarvis anything…"
                  }
                  spellCheck={false}
                  autoComplete="off"
                />
                <button type="submit" className="send" aria-label="Send">
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
