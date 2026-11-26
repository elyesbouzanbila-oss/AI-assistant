import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type {
  BridgeEvent,
  ConfirmRequest,
  ConnectionReport,
  Phase,
  SettingsDraft,
  SettingsView,
} from "./types";

/**
 * True when running inside the Tauri shell. In that case every call goes to
 * Rust. In a plain browser (dev preview) we run a scripted demo instead so the
 * UI can be inspected without a desktop behind it.
 */
export const isLive =
  typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

let demoEmit: ((event: BridgeEvent) => void) | null = null;
const demoPending = new Map<string, (allow: boolean) => void>();

export function subscribe(handler: (event: BridgeEvent) => void): () => void {
  if (!isLive) {
    demoEmit = handler;
    return () => {
      demoEmit = null;
    };
  }

  const unlisten: Array<() => void> = [];
  let closed = false;

  const wire = async () => {
    const offPhase = await listen<{ phase: Phase; label?: string }>(
      "agent:phase",
      (e) => handler({ type: "phase", ...e.payload }),
    );
    const offDelta = await listen<{ text: string }>("agent:delta", (e) =>
      handler({ type: "delta", text: e.payload.text }),
    );
    const offConfirm = await listen<ConfirmRequest>("agent:confirm", (e) =>
      handler({ type: "confirm", request: e.payload }),
    );
    const offFinal = await listen<{ text: string }>("agent:final", (e) =>
      handler({ type: "final", text: e.payload.text }),
    );
    const offError = await listen<{ message: string }>("agent:error", (e) =>
      handler({ type: "error", message: e.payload.message }),
    );
    const offFocus = await listen("agent:focus-input", () =>
      handler({ type: "focus" }),
    );
    const offDictate = await listen("agent:dictate", () =>
      handler({ type: "dictate" }),
    );

    const offs = [
      offPhase,
      offDelta,
      offConfirm,
      offFinal,
      offError,
      offFocus,
      offDictate,
    ];
    if (closed) {
      offs.forEach((off) => off());
      return;
    }
    unlisten.push(...offs);
  };

  void wire().catch((err) => {
    handler({ type: "error", message: `Bridge failed: ${String(err)}` });
  });

  return () => {
    closed = true;
    unlisten.forEach((f) => f());
  };
}

/**
 * `voice` marks a turn whose prompt is still exactly the dictation transcript;
 * Rust speaks that answer and stays silent for every typed turn.
 */
export async function ask(prompt: string, voice = false): Promise<void> {
  if (isLive) {
    await invoke("ask", { prompt, voice });
    return;
  }
  void runDemo(prompt);
}

export async function confirm(id: string, allow: boolean): Promise<void> {
  if (isLive) {
    await invoke("confirm", { id, allow });
    return;
  }
  const resolve = demoPending.get(id);
  demoPending.delete(id);
  resolve?.(allow);
}

export async function startDictation(): Promise<void> {
  if (isLive) {
    await invoke("start_dictation");
    return;
  }
  await sleep(300);
}

/** Resolves with the transcript, ready to drop into the composer. */
export async function stopDictation(): Promise<string> {
  if (isLive) {
    return invoke<string>("stop_dictation");
  }
  await sleep(900);
  return "Open Notepad and type a summary of my screen";
}

export async function getSettings(): Promise<SettingsView> {
  if (isLive) {
    return invoke<SettingsView>("get_settings");
  }
  await sleep(200);
  return { ...demoSettings };
}

/**
 * Saves and applies at once. A rejected configuration comes back in
 * `view.error` rather than as a rejection, so the panel keeps the typed values.
 */
export async function testSettings(
  draft: SettingsDraft,
): Promise<ConnectionReport> {
  if (isLive) {
    return invoke<ConnectionReport>("test_settings", { draft });
  }
  await sleep(600);
  if (!draft.baseUrl.trim().startsWith("http")) {
    throw new Error(`could not reach ${draft.baseUrl || "(no endpoint)"} `);
  }
  return {
    latencyMs: 218,
    models: [
      "google/gemini-2.5-flash",
      "openai/gpt-4o-mini",
      "minicpm5:latest",
      "glm-4.6v",
    ],
  };
}

export async function saveSettings(
  draft: SettingsDraft,
): Promise<SettingsView> {
  if (isLive) {
    return invoke<SettingsView>("save_settings", { draft });
  }
  await sleep(450);
  const error = draft.baseUrl.trim().startsWith("http")
    ? null
    : "The endpoint must start with http:// or https://.";
  demoSettings = {
    ...demoSettings,
    baseUrl: draft.baseUrl,
    model: draft.model,
    maxSteps: draft.maxSteps,
    sttProvider: draft.sttProvider,
    whisperBin: draft.whisperBin,
    whisperModel: draft.whisperModel,
    sttModel: draft.sttModel,
    sttBaseUrl: draft.sttBaseUrl,
    fallbackBaseUrl: draft.fallbackBaseUrl,
    fallbackModel: draft.fallbackModel,
    speak: draft.speak,
    vision: draft.vision,
    voice: draft.voice,
    speechRate: draft.speechRate,
    hasApiKey: draft.clearApiKey
      ? false
      : demoSettings.hasApiKey || Boolean(draft.apiKey.trim()),
    hasSttApiKey: draft.clearSttApiKey
      ? false
      : demoSettings.hasSttApiKey || Boolean(draft.sttApiKey.trim()),
    error,
  };
  return { ...demoSettings };
}

export async function cancel(): Promise<void> {
  if (!isLive) {
    demoPending.forEach((resolve) => resolve(false));
    demoPending.clear();
    demoEmit?.({ type: "phase", phase: "idle" });
    return;
  }
  await invoke("cancel");
}

/**
 * Only "idle" and "open" are sent from the UI: they are pure window-state
 * changes. Everything else is driven by Rust so the UI cannot disagree with
 * what the agent is actually doing.
 */
export async function setPhase(
  phase: Extract<Phase, "idle" | "open" | "settings">,
): Promise<void> {
  if (!isLive) return;
  await invoke("ui_phase", { phase });
}

// ---------------------------------------------------------------------------
// Demo mode - scripted so the preview shows the real interaction model.
// ---------------------------------------------------------------------------

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

let demoSettings: SettingsView = {
  baseUrl: "https://openrouter.ai/api/v1",
  model: "google/gemini-2.5-flash",
  maxSteps: 8,
  sttProvider: "model",
  whisperBin: "",
  whisperModel: "",
  sttModel: "",
  sttBaseUrl: "",
  fallbackBaseUrl: "http://localhost:11434/v1",
  fallbackModel: "minicpm5:latest",
  speak: true,
  vision: true,
  voice: "",
  speechRate: 0,
  hasApiKey: true,
  hasSttApiKey: false,
  envLocked: [],
  error: null,
};

function askDemo(request: Omit<ConfirmRequest, "id">): Promise<boolean> {
  const id = `demo-${Math.random().toString(36).slice(2)}`;
  return new Promise((resolve) => {
    demoPending.set(id, resolve);
    demoEmit?.({ type: "confirm", request: { ...request, id } });
  });
}

async function runDemo(prompt: string): Promise<void> {
  const emit = demoEmit;
  if (!emit) return;

  const wants = (re: RegExp) => re.test(prompt);

  emit({ type: "phase", phase: "working", label: "Thinking" });
  await sleep(650);

  if (wants(/screen|see|look|error|what.?s on/i)) {
    emit({ type: "phase", phase: "working", label: "Reading your screen" });
    await sleep(900);
    emit({
      type: "delta",
      text: "I can see your desktop: one browser window on the left and a Notepad window on the right.",
    });
    emit({ type: "final", text: "Screenshot captured and sent to the model." });
    return;
  }

  if (wants(/open|launch|start|notepad|spotify|calc/i)) {
    const app = /notepad/i.test(prompt) ? "Notepad" : "Spotify";
    const allowed = await askDemo({
      tool: "open_app",
      summary: `Open "${app}"`,
    });
    if (!allowed) {
      emit({ type: "final", text: "Cancelled - nothing was opened." });
      return;
    }
    emit({ type: "phase", phase: "working", label: `Opening ${app}` });
    await sleep(1100);
    emit({ type: "delta", text: `${app} is open and focused.` });
    emit({ type: "final", text: `Opened ${app}.` });
    return;
  }

  if (wants(/type|write|note|paragraph/i)) {
    const allowed = await askDemo({
      tool: "type_text",
      summary: 'Type 214 characters into "Untitled - Notepad"',
    });
    if (!allowed) {
      emit({ type: "final", text: "Cancelled - nothing was typed." });
      return;
    }
    emit({ type: "phase", phase: "working", label: "Typing" });
    await sleep(900);
    emit({ type: "final", text: "Typed it into the focused window." });
    return;
  }

  emit({
    type: "delta",
    text: "I can open and close apps, focus windows, type, click, read your screen and work with files in my workspace. This is the browser demo - the real app does all of it on your PC.",
  });
  emit({ type: "final", text: "Demo mode: no desktop control in the browser preview." });
}
