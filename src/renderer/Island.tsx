import { useCallback, useEffect, useRef, useState } from "react";
import {
  ask,
  cancel,
  confirm,
  isLive,
  setPhase as pushPhase,
  startDictation,
  stopDictation,
  subscribe,
} from "./lib/bridge";
import type { BridgeEvent, ChatLine, ConfirmRequest, Phase } from "./lib/types";

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
      if (dictatingRef.current) void finishDictation(false);
      void cancel();
      collapse();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [collapse, finishDictation]);

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
            <button className="icon" onClick={close} aria-label="Close">
              &times;
            </button>
          )}
        </header>

        <div className="body">
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
