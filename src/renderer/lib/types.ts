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
  sttModel: string;
  sttBaseUrl: string;
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
  sttModel: string;
  sttBaseUrl: string;
  /** Blank keeps the stored key. */
  apiKey: string;
  sttApiKey: string;
}

/** Pixel size of the island per phase. Mirrored in src-tauri/src/island.rs. */
export const PHASE_SIZE: Record<Phase, { w: number; h: number }> = {
  idle: { w: 232, h: 48 },
  open: { w: 560, h: 248 },
  settings: { w: 560, h: 452 },
  working: { w: 560, h: 96 },
  confirm: { w: 560, h: 172 },
  done: { w: 560, h: 72 },
  error: { w: 560, h: 132 },
};
