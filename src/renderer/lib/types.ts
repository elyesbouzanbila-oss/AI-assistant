export type Phase =
  | "idle"
  | "open"
  | "settings"
  | "working"
  | "confirm"
  | "done"
  | "error";

export interface ConfirmRequest {
  id: string;
  tool: string;
  /** Human-readable one-liner shown in the island, e.g. `Open "Notepad"`. */
  summary: string;
}

export type BridgeEvent =
  | { type: "phase"; phase: Phase; label?: string }
  /** Rust asked for the composer (Ctrl+Space). */
  | { type: "focus" }
  /** Ctrl+Shift+Space: start or stop dictation. */
  | { type: "dictate" }
  | { type: "delta"; text: string }
  | { type: "confirm"; request: ConfirmRequest }
  | { type: "final"; text: string }
  | { type: "error"; message: string };

export interface ChatLine {
  id: string;
  role: "user" | "assistant";
  text: string;
}

/**
 * What the settings panel may see. It never carries a key: only whether one is
 * stored, so the field starts blank and typing replaces it.
 */
export interface SettingsView {
  baseUrl: string;
  model: string;
  maxSteps: number;
  /** `model` or `windows`: who transcribes dictated audio. */
  sttProvider: string;
  whisperBin: string;
  whisperModel: string;
  sttModel: string;
  sttBaseUrl: string;
  fallbackBaseUrl: string;
  fallbackModel: string;
  /** Read dictated answers aloud. */
  speak: boolean;
  /** Offer the screenshot tool; off for a model that rejects images. */
  vision: boolean;
  /** SAPI voice name; blank means the system default. */
  voice: string;
  speechRate: number;
  hasApiKey: boolean;
  hasSttApiKey: boolean;
  /** Variables set in `.env`; those fields are read-only until removed there. */
  envLocked: string[];
  /** Why the saved configuration was rejected, if it was. */
  error: string | null;
}

export interface SettingsDraft {
  baseUrl: string;
  model: string;
  maxSteps: number;
  /** `model` or `windows`. */
  sttProvider: string;
  whisperBin: string;
  whisperModel: string;
  sttModel: string;
  sttBaseUrl: string;
  /** Local model used when the endpoint above fails; blank disables it. */
  fallbackBaseUrl: string;
  fallbackModel: string;
  /** Blank keeps the stored key. */
  apiKey: string;
  sttApiKey: string;
  /** Explicit removal, since a blank field cannot express it. */
  clearApiKey: boolean;
  clearSttApiKey: boolean;
  speak: boolean;
  vision: boolean;
  voice: string;
  speechRate: number;
}

/** What the panel's test button learns without spending tokens. */
export interface ConnectionReport {
  latencyMs: number;
  models: string[];
}

/** Pixel size of the island per phase. Mirrored in src-tauri/src/island.rs. */
export const PHASE_SIZE: Record<Phase, { w: number; h: number }> = {
  idle: { w: 232, h: 48 },
  open: { w: 560, h: 248 },
  settings: { w: 560, h: 700 },
  working: { w: 560, h: 96 },
  confirm: { w: 560, h: 172 },
  done: { w: 560, h: 72 },
  error: { w: 560, h: 132 },
};
