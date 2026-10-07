// Event names + payload types shared with the Rust backend.
// Keep in sync with src-tauri/src/events.rs and models/downloader.rs.

export const STATE_CHANGED = "state://changed";
/**
 * Short status line for the pill ("Polish…"), cleared with an empty string.
 * Never carries transcript text — the pill deliberately shows no live
 * transcription.
 */
export const OVERLAY_STATUS = "overlay://status";
export const TRANSCRIPT_FINAL = "transcript://final";
export const LEVEL = "overlay://level";
export const NOTICE_ERROR = "notice://error";
export const HOTKEY_CAPTURE = "hotkey://capture";
export const MODEL_PROGRESS = "models://progress";
export const NAVIGATE = "nav://page";
/**
 * The backend changed the settings file on its own initiative (today: a
 * learned correction being promoted to a replacement rule), so this window's
 * copy is stale and must be re-read before it writes again. No payload —
 * reload the whole object, because the point is that we don't know what
 * changed.
 */
export const SETTINGS_CHANGED = "settings://changed";

/**
 * Updater state, re-emitted on every transition; `updateStatus()` returns
 * the same shape for a page that mounts mid-flight. Mirrors
 * `updater::UpdateState` in src-tauri/src/updater.rs — keep the two in sync.
 */
export const UPDATE_STATE = "update://state";

/**
 * A Butterfly Labs sign-in finished, failed, or was signed out. Carries the
 * same `CloudStatus` shape `cloudStatus()` returns, so a listener never has to
 * ask again. Mirrors `events::CLOUD_AUTH_CHANGED` in src-tauri/src/events.rs.
 *
 * The name has no `://` on purpose: a scheme separator in it would read as
 * one more URL in a sign-in flow that already has several real ones.
 */
export const CLOUD_AUTH_CHANGED = "cloud-auth-changed";

export type UpdateStatePayload =
  | { kind: "idle" }
  | { kind: "checking" }
  | { kind: "upToDate" }
  | { kind: "available"; version: string; notes: string | null }
  | { kind: "downloading"; version: string; percent: number | null }
  | { kind: "installing"; version: string }
  | { kind: "error"; message: string };

export type DictationState = "idle" | "recording" | "finalizing" | "injecting";

export interface StatePayload {
  state: DictationState;
  mode: "pushToTalk" | "handsFree" | null;
}

export interface StatusPayload {
  text: string;
}

export interface FinalPayload {
  text: string;
  /** Length of the captured audio, for words-per-minute stats. */
  durationMs: number;
  /** The program the text was pasted into, by its lowercased executable
   * name without ".exe" (`notepad`). */
  app: string | null;
  /** Word-level edits made by cleanup + polish. */
  wordsCorrected: number;
  /** Dictionary rules + snippet expansions that fired. */
  dictFixes: number;
}

export interface LevelPayload {
  level: number;
}

export interface NoticePayload {
  message: string;
}

export interface HotkeyCapturePayload {
  keys: string;
  done: boolean;
  /** Present when a press was ignored; recording continues. */
  hint?: string;
}

export interface NavigatePayload {
  page: string;
}

export interface ModelProgressPayload {
  id: string;
  phase: "downloading" | "verifying" | "extracting" | "done" | "error" | "cancelled";
  downloaded: number;
  total: number;
  bytesPerSec: number;
  message: string | null;
}

/** The whole import queue, re-emitted on every state change. A snapshot, not a
 * delta — `importStatus()` returns the same shape for a page that mounts
 * mid-run. */
export const IMPORT_PROGRESS = "import://progress";

/** Whether files are being dragged over the window. The dropped *paths* never
 * reach this webview: Rust handles the drop and enqueues directly, so this
 * carries only the highlight state. */
export const IMPORT_DROP_HOVER = "import://drop-hover";

export type ImportItemState =
  | "queued"
  | "probing"
  /** Decoding the recording to the 16 kHz mono WAV Sarvam's batch endpoint
   * will actually transcribe. Its own state because its length is set by the
   * machine rather than the network — a long recording is minutes of CPU. */
  | "converting"
  | "uploading"
  | "transcribing"
  | "done"
  | "failed"
  | "cancelled";

export interface ImportItemPayload {
  id: number;
  /** The file's own name, never its directory. */
  name: string;
  state: ImportItemState;
  /** A finished sentence, present only when `state` is `"failed"`. */
  error: string | null;
  /** A remark that is not a failure — today only "Length unknown", when the
   * container states no playing time. Two things follow from it: the duration
   * ceiling could not be applied, and the job waits on the 30-minute ceiling
   * rather than a length-scaled deadline. */
  detail: string | null;
  /** The note this import became, once it has become one. */
  noteId: number | null;
}

/** Counts and percent only — never bytes, never a timer, never transcript text.
 * `percent` counts finished rows of every outcome (done, failed and cancelled
 * alike), so a finished run always shows 100%; it changes only when a row
 * finishes. */
export interface ImportProgressPayload {
  running: boolean;
  total: number;
  done: number;
  failed: number;
  cancelled: number;
  percent: number;
  items: ImportItemPayload[];
}

export interface ImportDropHoverPayload {
  over: boolean;
}
