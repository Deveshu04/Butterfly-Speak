// Typed wrappers for every Tauri command. Keep in sync with
// src-tauri/src/commands.rs.

import { invoke } from "@tauri-apps/api/core";
// `import_status` answers in the same shape the `import://progress` event
// carries, so the type is imported from there rather than restated — two
// copies of a payload shape is how a listener and a poller drift apart.
import type { ImportProgressPayload, UpdateStatePayload } from "$lib/events";

export interface Snippet {
  trigger: string;
  expansion: string;
}

export interface Replacement {
  from: string;
  to: string;
  /** Learned automatically — either from an edit in the history feed, or,
   * once it has been seen in two separate paste sessions, from a correction
   * made in the target app (`learn::candidates`). Undo one of these with
   * `undoLearnedCorrection`, never a plain settings write: the rule is only
   * half of what the app stored. */
  auto: boolean;
}

export interface StyleRule {
  /** Case-insensitive substring of the focused process name ("whatsapp"). */
  app: string;
  style: string;
}

export interface Transform {
  name: string;
  prompt: string;
  /** Chord string like "Win+Alt+1". */
  shortcut: string;
}

export type StylePreset = "formal" | "casual" | "veryCasual";

/** How aggressively AI formatting may rewrite. Mirrors `CleanupLevel` in
 * src-tauri/src/format/level.rs — keep the two in sync. */
export type CleanupLevel = "off" | "light" | "balanced" | "high";

/** Which engine a dictation uses. Mirrors `settings::Provider`:
 * `"cloud"` is Butterfly Labs' relay, `"sarvam"` is the user's own Sarvam key
 * ("bring your own key"), `"local"` is the on-device model. */
export type Provider = "cloud" | "sarvam" | "local";

export interface Settings {
  version: number;
  provider: Provider;
  hotkey: { binding: string; doubleTapMs: number; minHoldMs: number };
  model: { selectedId: string };
  sarvam: {
    languageCode: string;
    streamType: "fast" | "balanced";
    mode: string;
    polishModel: string;
  };
  audio: { deviceName: string | null; cues: boolean; pauseMedia: boolean };
  cleanup: {
    spokenCommands: boolean;
    fillers: boolean;
    aggressiveFillers: boolean;
    backtrack: boolean;
    punctuation: boolean;
    itn: boolean;
    /** How aggressively AI formatting may rewrite. */
    level: CleanupLevel;
  };
  dictionary: string[];
  replacements: Replacement[];
  snippets: Snippet[];
  style: StylePreset | string;
  styleRules: StyleRule[];
  transforms: Transform[];
  transformsEnabled: boolean;
  shortcuts: {
    pasteLast: string;
    copyLast: string;
    scratchpad: string;
    undoAiEdit: string;
    /** Dictation-grade chord: dictate, then translate before pasting. */
    translateDictation: string;
    /** Dictation-grade chord: the dictation is a command for the agent. */
    voiceAgent: string;
  };
  dictation: { smartSpace: boolean };
  injection: {
    restoreClipboard: boolean;
    /** Milliseconds between the paste and putting the user's clipboard back.
     * Clamped to 0..=1000 by `settings::repair` on load and on import — the
     * replace step is spent inside the finalize watchdog, which is budgeted
     * against that maximum (`settings::RESTORE_DELAY_MAX_MS`). No UI binds
     * this today; anything larger written here is silently clamped. */
    restoreDelayMs: number;
  };
  overlay: { offsetY: number };
  app: { launchAtLogin: boolean; onboardingDone: boolean };
  history: HistorySettings;
  translation: TranslationSettings;
  agent: AgentSettings;
  learn: LearnSettings;
  notes: NotesSettings;
  prompts: PromptOverrides;
  customEndpoint: CustomEndpointSettings;
  /** Auto-update policy. Mirrors `settings::UpdateSettings`. */
  updates: { autoCheck: boolean };
}

/** Mirrors `settings::NotesSettings`. */
export interface NotesSettings {
  /** Write a `.md` file per note into `mirrorDir`. Ships off. */
  mirrorEnabled: boolean;
  /** Where the mirror writes. `null` until the user picks a folder — and an
   * enabled mirror with no folder writes nothing, rather than inventing a
   * location the user never chose. */
  mirrorDir: string | null;
}

/** The prompts the Prompts page can edit. Mirrors `settings::EditablePrompt`
 * — keep the two in sync. */
export type EditablePrompt = "light" | "balanced" | "high" | "agent" | "selectionRules";

/** Mirrors `settings::PromptOverrides`.
 *
 * `null` means "send whichever prompt this build ships", so a kind the user
 * never edited moves with app updates. Saving text equal to the current
 * default stores `null` — the Rust side enforces that on every write
 * (`PromptOverrides::normalize`), so the UI never has to be the only guard.
 *
 * Only the RULES half of each prompt lives here. The injection-hardening
 * stanza, the agent's output rules, the transcript-delimiter line and the
 * end-marker rule are re-appended around this text by the
 * backend and are not editable. */
export interface PromptOverrides {
  light: string | null;
  balanced: string | null;
  high: string | null;
  agent: string | null;
  selectionRules: string | null;
}

/** One kind's shipped rules text — what "Reset to default" restores. */
export interface PromptDefault {
  kind: EditablePrompt;
  defaultRules: string;
}

/** What a live prompt test produced. Both outputs are reported: the guardrail
 * can substitute the rule-cleaned text for the model's reply, and a user
 * tuning a prompt who is shown only the substituted text is debugging the
 * wrong string. */
export interface PromptTestResult {
  /** The exact system turn that was sent. */
  systemPrompt: string;
  /** The model's reply, before the guardrail. */
  rawOutput: string;
  /** What the app would actually have used. */
  finalOutput: string;
  /** The pill copy the dictation would have carried, if any. */
  notice: string | null;
  truncated: boolean;
  /** False when the guardrail (or a truncation) discarded the reply. */
  usedModelOutput: boolean;
}

/** Mirrors `settings::CustomEndpointSettings`.
 *
 * **No key field, and there will never be one.** The endpoint's credential
 * lives in the Windows credential store; `Settings` is what
 * `exportSettings` writes to a user-chosen JSON file. */
export interface CustomEndpointSettings {
  /** Exactly as typed — normalized backend-side at use time, so a half-typed
   * URL is never rewritten under the cursor. Empty = the slot is off. */
  baseUrl: string;
  /** The chat model, sent as typed: `/v1/models` only suggests ids, and an
   * unlisted one may still work. */
  model: string;
  /** The transcription model, for the `/audio/transcriptions` half. A second
   * field because the two halves of one host almost never answer to the same
   * name, and sending the chat id to the transcription route is a 400 from
   * every server. Empty sends `whisper-1`, OpenAI's id for its Whisper
   * model, which OpenAI-style transcription servers usually accept. */
  sttModel: string;
  /** Route AI formatting / agent / transform calls here. */
  useForPolish: boolean;
  /** Route speech-to-text here: the dictation records the whole utterance and
   * posts it to `{base}/audio/transcriptions`, instead of streaming to Sarvam
   * or running the on-device model. */
  useForStt: boolean;
}

/** Mirrors `settings::LearnSettings`. */
export interface LearnSettings {
  /** Whether the app reads back the field it just pasted into, to notice a
   * word the user corrected by hand. Ships on. */
  fieldMonitorEnabled: boolean;
}

/** Mirrors `settings::TranslationSettings`. */
export interface TranslationSettings {
  /** Sarvam language code to translate into. Blank = not configured, and the
   * translate chord skips the translation and pastes the cleaned text with a
   * notice. */
  targetLanguage: string;
}

/** Mirrors `settings::AgentSettings`. */
export interface AgentSettings {
  /** What the user calls the voice agent; also its wake word. */
  name: string;
  /** Whether saying the agent's name mid-dictation addresses it. Ships off. */
  wakeWordEnabled: boolean;
}

export interface HistorySettings {
  /** Master switch: recording is a write-time no-op while this is off. */
  enabled: boolean;
  /** Days to keep a transcription before the retention sweep purges it.
   * 0 = forever. */
  keepDays: number;
}

/** Mirrors `history::Outcome` (src-tauri/src/history/entry.rs): `"done"`
 * when the text was delivered, `"failed"` when nothing was typed. */
export type HistoryOutcome = "done" | "failed";

/** Mirrors `history::Entry` — a stored row. */
export interface HistoryEntry {
  id: number;
  text: string;
  rawText: string | null;
  /** `datetime('now')`-formatted UTC string ("YYYY-MM-DD HH:MM:SS"). */
  createdAt: string;
  outcome: HistoryOutcome;
  errorCode: string | null;
  provider: string | null;
  model: string | null;
  durationMs: number | null;
  app: string | null;
  words: number | null;
  /** `null` for a plain dictation, otherwise the route it took
   * ("translation", "agent"). */
  route: string | null;
}

/** Where a note came from: the editor, or an imported recording. */
export type NoteKind = "written" | "imported";

/** Mirrors `notes::Note` (src-tauri/src/notes/mod.rs). */
export interface Note {
  id: number;
  /** `null` is "unfiled", which is a real state here — nothing seeds a
   * default folder for a note to be hidden in. */
  folderId: number | null;
  kind: NoteKind;
  title: string;
  content: string;
  /** The last note action's result, kept beside `content`. */
  polishedBody: string | null;
  /** The action prompt that produced `polishedBody`. */
  polishPrompt: string | null;
  /** A fingerprint of the content `polishedBody` was made from. */
  polishedFromHash: string | null;
  /** The file import's segment array, verbatim JSON. Read-only through
   * `updateNote` — it and the text FTS indexes have to move together. */
  transcriptJson: string | null;
  /** The recording an import was made from, and its length. */
  importedFile: string | null;
  audioSeconds: number | null;
  /** Epoch **milliseconds** — `new Date(createdAt)` directly. Unlike
   * `HistoryEntry.createdAt` these need no `parseUtc` fixup. */
  createdAt: number;
  updatedAt: number;
}

/** Mirrors `notes::Folder`. Folders are flat — there is no `parentId`. */
export interface Folder {
  id: number;
  name: string;
  sortOrder: number;
  createdAt: number;
  updatedAt: number;
  /** How many notes are filed here, counted with the row. */
  noteCount: number;
}

/** Mirrors `notes::NewNote`. Every field is optional; the defaults are
 * `kind: "written"` and empty title/content. */
export interface NewNote {
  folderId?: number | null;
  kind?: NoteKind;
  title?: string;
  content?: string;
  transcriptJson?: string | null;
  importedFile?: string | null;
  audioSeconds?: number | null;
  /** Epoch-ms overrides for content that arrives carrying its own history —
   * the Scratchpad migration, which must not stamp weeks of notes with
   * today's date. Absent means "now"; `createdAt` alone also sets
   * `updatedAt`. There is no matching field on `NoteUpdate`: an import states
   * when a note was written, an edit never rewrites it. */
  createdAt?: number;
  updatedAt?: number;
}

/** Mirrors `notes::NoteUpdate` — the update allow-list, as a type.
 *
 * Omitting a field leaves the column alone; passing `null` to one of the
 * nullable fields **clears** it. That distinction is the only way to say
 * "unfile this note" (`{ folderId: null }`) or "drop the action's result",
 * so do not spread a whole `Note` in here — send just what changed. */
export interface NoteUpdate {
  title?: string;
  content?: string;
  polishedBody?: string | null;
  polishPrompt?: string | null;
  polishedFromHash?: string | null;
  folderId?: number | null;
}

/** Mirrors `notes::ListNotesArgs`. `folder` omitted lists every note, `null`
 * lists the unfiled ones, a number lists one folder. */
export interface ListNotesArgs {
  folder?: number | null;
  page?: number;
}

export interface RamVerdict {
  kind: "ok" | "caution" | "notRecommended";
  message?: string;
}

export interface ModelStatus {
  id: string;
  displayName: string;
  tier: "light" | "balanced" | "accurate" | "max";
  description: string;
  engine: string;
  dirName: string;
  diskBytes: number;
  estRamBytes: number;
  werPct: number;
  nativePunct: boolean;
  installed: boolean;
  selected: boolean;
  downloading: boolean;
  verdict: RamVerdict;
}

export interface PolishStatus {
  /** False when the app was built without the on-device polish engine. */
  available: boolean;
  installed: boolean;
  downloading: boolean;
  diskBytes: number;
  estRamBytes: number;
  verdict: RamVerdict;
}

export interface SystemInfo {
  totalRamBytes: number;
  availableRamBytes: number;
  appVersion: string;
}

export interface SarvamKeyStatus {
  present: boolean;
  /** "••••" + last 4 characters when a key is stored. */
  masked: string | null;
}

/** Mirrors `commands::CustomEndpointKeyStatus`. Same mask shape as
 * `SarvamKeyStatus` — one rule for every key the Settings screen shows. */
export interface CustomEndpointKeyStatus {
  present: boolean;
  masked: string | null;
}

/** One entry from a `/models` response. `ownedBy` is the host's own label,
 * shown as a sublabel and used for nothing else. */
export interface EndpointModel {
  id: string;
  ownedBy: string | null;
}

/** Mirrors `commands::EndpointProbeResult`. */
export interface EndpointProbeResult {
  /** The URL that was actually requested — how the user learns that
   * "https://host" became "https://host/v1/models". */
  url: string;
  models: EndpointModel[];
}

export const getSettings = () => invoke<Settings>("get_settings");
export const setSettings = (settings: Settings) =>
  invoke<void>("set_settings", { settings });
export const listModels = () => invoke<ModelStatus[]>("list_models");
export const polishStatus = () => invoke<PolishStatus>("polish_status");
export const downloadModel = (id: string) => invoke<void>("download_model", { id });
export const cancelDownload = (id: string) => invoke<void>("cancel_download", { id });
export const deleteModel = (id: string) => invoke<void>("delete_model", { id });
export const selectModel = (id: string) => invoke<void>("select_model", { id });
export const listMics = () => invoke<string[]>("list_mics");
export const systemInfo = () => invoke<SystemInfo>("system_info");
export const setMeter = (enabled: boolean) => invoke<void>("set_meter", { enabled });
export const hotkeyCapture = (active: boolean) =>
  invoke<void>("hotkey_capture", { active });
export const ensureSupportModels = () => invoke<void>("ensure_support_models");
export const sarvamKeyStatus = () => invoke<SarvamKeyStatus>("sarvam_key_status");
export const setSarvamKey = (key: string) => invoke<void>("set_sarvam_key", { key });
/** Validates against the live Sarvam realtime endpoint; stores the key on success. */
export const validateSarvamKey = (key: string) =>
  invoke<void>("validate_sarvam_key", { key });

/** The shipped rules for every editable prompt. */
export const promptDefaults = () => invoke<PromptDefault[]>("prompt_defaults");
/** The exact system turn a kind would send, composed by the same builders the
 * dictation path uses. `rules` is the unsaved draft; omit it for what is
 * currently saved. Nothing is stored. */
export const previewPrompt = (kind: EditablePrompt, rules: string | null) =>
  invoke<string>("preview_prompt", { kind, rules });
/** Run a draft prompt against the live model. The draft travels as an argument
 * and is never written to settings — no persisted draft if the app dies
 * mid-request, and no other window can pick it up. `selection` is required for
 * the `selectionRules` kind and ignored by every other. */
export const testPrompt = (
  kind: EditablePrompt,
  rules: string | null,
  input: string,
  selection: string | null,
) => invoke<PromptTestResult>("test_prompt", { kind, rules, input, selection });
export const customEndpointKeyStatus = () =>
  invoke<CustomEndpointKeyStatus>("custom_endpoint_key_status");
/** Stores the custom endpoint's key, or removes it when `key` is empty.
 * There is no validation step: an OpenAI-compatible host may legitimately
 * need no key, so "no key" is a configuration, not an error. */
export const setCustomEndpointKey = (key: string) =>
  invoke<void>("set_custom_endpoint_key", { key });
/** Pre-flight only — no network call. Resolves to the full chat route that
 * would actually be requested (`{base}/v1/chat/completions`, with `/v1` added
 * only if missing and any query string kept at the end); rejects with the
 * sentence to show. */
export const checkCustomEndpoint = (url: string) =>
  invoke<string>("check_custom_endpoint", { url });
/** `GET {base}/models` with a 4 s budget. Rejects with prose that says which
 * of "unusable URL", "couldn't reach it", "it refused the credential" and
 * "it answered with an error" happened. The key is read from the credential
 * store backend-side and never passes through here. */
export const endpointTestConnection = (url: string) =>
  invoke<EndpointProbeResult>("endpoint_test_connection", { url });
/** The same probe, for the discovery dropdown. A payload the backend cannot
 * read resolves to an empty list — never an error, and never a reason to
 * touch the model id the user typed. */
export const endpointListModels = (url: string) =>
  invoke<EndpointModel[]>("endpoint_list_models", { url });

export const historyList = (page: number, pageSize: number) =>
  invoke<HistoryEntry[]>("history_list", { page, pageSize });
export const historySearch = (query: string, limit: number) =>
  invoke<HistoryEntry[]>("history_search", { query, limit });
export const historyDelete = (id: number) => invoke<boolean>("history_delete", { id });
/** Home's Edit: replace a row's text (and its word count). `false` when the
 * row is gone. */
export const historyUpdateText = (id: number, text: string) =>
  invoke<boolean>("history_update_text", { id, text });
export const historyClear = () => invoke<number>("history_clear");

/** Notes and folders. All of these run on the history DB thread; a session in
 * which that thread never started degrades to empty reads and a sentence on
 * the writes, rather than to an error the user caused.
 *
 * Notes are NOT under history retention: neither the retention sweep nor the
 * "Keep dictation history" switch touches them. They are documents. */
export const createNote = (note: NewNote) => invoke<number>("create_note", { note });
export const getNote = (id: number) => invoke<Note | null>("get_note", { id });
/** Sends only what changed — see `NoteUpdate` on why not to spread a `Note`.
 * Resolves false when the update named no fields or matched no row. */
export const updateNote = (id: number, update: NoteUpdate) =>
  invoke<boolean>("update_note", { id, update });
/** Hard delete: there is no trash, and the search index is cleaned with it. */
export const deleteNote = (id: number) => invoke<boolean>("delete_note", { id });
export const listNotes = (args: ListNotesArgs = {}) =>
  invoke<Note[]>("list_notes", { args });
/** `limit` defaults to `notes::SEARCH_LIMIT` (60) on the Rust side. */
export const searchNotes = (query: string, limit?: number) =>
  invoke<Note[]>("search_notes", { query, limit });
export const listFolders = () => invoke<Folder[]>("list_folders");
/** Rejects an empty name and a duplicate one (case-insensitively); the error
 * message is written to be shown as-is. */
export const createFolder = (name: string) => invoke<Folder>("create_folder", { name });
export const renameFolder = (id: number, name: string) =>
  invoke<Folder>("rename_folder", { id, name });
/** Deletes the folder and every note in it, both or neither. Resolves the ids
 * of the notes that went with it. */
export const deleteFolder = (id: number) => invoke<number[]>("delete_folder", { id });

/** A note AI action: a prompt run over a note's body, whose result is stored
 * in `polishedBody` rather than replacing what was written. Mirrors
 * `notes::actions::NoteAction`; each field is named after its column. */
export interface NoteAction {
  id: number;
  /** `null` for a user's action; for a shipped one, the key seeding
   * matches on, so it is never inserted twice. */
  shippedKey: string | null;
  /** Shipped with the app: editable, never deletable. Hide the delete
   * control for these — the Rust side refuses as well, and both halves are
   * meant to be there. */
  shipped: boolean;
  /** Menu order, lowest first; ties go to the older action. */
  position: number;
  /** The name shown in the Enhance menu. */
  label: string;
  /** The line under the label. */
  summary: string;
  /** What the model is asked to do with the note. */
  instruction: string;
  /** An `Icon` name. New actions default to "note". */
  glyph: string;
  /** Epoch milliseconds. */
  createdAt: number;
  updatedAt: number;
}

export interface NewNoteAction {
  label: string;
  instruction: string;
  summary?: string;
  /** Defaults to "note" on the Rust side. */
  glyph?: string;
}

/** Everything an edit can change. Send only what changed; a field left out
 * keeps its value. There is no field for `shipped` or `shippedKey`, so an
 * edit cannot touch either. */
export interface NoteActionUpdate {
  position?: number;
  label?: string;
  summary?: string;
  instruction?: string;
  glyph?: string;
}

/** Every action, in menu order. Seeds the shipped ones on first call. */
export const listNoteActions = () => invoke<NoteAction[]>("list_note_actions");
export const createNoteAction = (action: NewNoteAction) =>
  invoke<NoteAction>("create_note_action", { action });
export const updateNoteAction = (id: number, update: NoteActionUpdate) =>
  invoke<boolean>("update_note_action", { id, update });
/** Rejects a built-in with a sentence written to be shown as-is. */
export const deleteNoteAction = (id: number) =>
  invoke<boolean>("delete_note_action", { id });

/** Run an action over the note's body and store the result. Resolves the note
 * as it was actually written — `polishedBody`, `polishPrompt` and
 * `polishedFromHash` all move together — so replace your copy with it
 * rather than patching one field.
 *
 * Rejects with a sentence written to be shown as-is. Takes as long as the
 * model does — up to ~60s on a note near the 3,000-word ceiling — so keep the
 * button in a pending state. */
export const runNoteAction = (noteId: number, actionId: number) =>
  invoke<Note>("run_note_action", { noteId, actionId });

/** Ask the model for a title, store it, and resolve it.
 *
 * Rejects — never resolves an empty string — when the call fails or the reply
 * has no usable title in it, so show the rejection as a notice: a broken
 * title model must never fail silently. */
export const generateNoteTitle = (noteId: number) =>
  invoke<string>("generate_note_title", { noteId });

/** Take back a correction the app learned by itself: removes the `auto`
 * replacement AND zeroes the evidence behind it, both or neither.
 *
 * Use this for any rule with `auto: true` instead of editing
 * `settings.replacements` directly. A promotion wrote two things — the rule,
 * and a row in the learn-candidate store — and after dropping only the rule,
 * the next single observation would re-promote what the user just rejected.
 * Resolves false when nothing changed. Settings are written on the Rust side,
 * so reload the store afterwards. */
export const undoLearnedCorrection = (from: string, to: string) =>
  invoke<boolean>("undo_learned_correction", { from, to });

/** Save one note to a file the user picks. Resolves false (not an error) if
 * they cancel.
 *
 * BOTH formats export the same document — the enhanced body when the note has
 * one. Choosing "txt" strips the markdown from that same text; it never falls
 * back to the raw pre-AI draft. */
export const exportNote = (id: number, format: "md" | "txt") =>
  invoke<boolean>("export_note", { id, format });

/** Opens a folder picker for the markdown mirror. Resolves null if the user
 * cancels. Only answers — write the path into `settings.notes.mirrorDir`
 * yourself, so choosing a folder is an ordinary settings save. */
export const pickNotesMirrorDir = () =>
  invoke<string | null>("pick_notes_mirror_dir");

/** Write every note to the mirror and resolve how many landed. Rejects with a
 * sentence when the mirror is off or has no folder. */
export const rebuildNotesMirror = () => invoke<number>("rebuild_notes_mirror");

/** Mirror the *resolved* theme ("light" or "dark", never "auto") into a
 * sidecar file beside settings.json, so the next launch can fill the window
 * with the right colour before the webview has painted anything.
 *
 * The preference itself stays in localStorage — that is what lets
 * `src/app.html` stamp it before first paint — but `setup()` runs before any
 * webview exists and cannot read WebView2 storage, so this is the one piece
 * that has to cross into Rust. See `src-tauri/src/theme.rs`.
 *
 * Called by `$lib/theme.svelte`, and only from the `main` window: the overlay
 * is granted no app command at all (`capabilities/overlay.json`), so a call
 * from the pill would be an ACL rejection. Rejects on an unwritable
 * `%APPDATA%`, which costs one white launch frame and nothing else. */
export const persistResolvedTheme = (resolved: "light" | "dark") =>
  invoke<void>("persist_resolved_theme", { resolved });

/** Opens a save dialog; resolves false (not an error) if the user cancels.
 * Never includes the Sarvam API key — it isn't part of `Settings`. */
export const exportSettings = () => invoke<boolean>("export_settings");
/** Opens an open-file dialog; resolves false (not an error) if the user
 * cancels. Runs through the same migrate/repair pipeline every load does. */
export const importSettings = () => invoke<boolean>("import_settings");

/** Audio/video file import.
 *
 * There is no `importAdd(path)` and there never should be: this webview is
 * never given a file path, so it cannot name one for the backend to read.
 * The picker runs its dialog in Rust and queues what it returns; drag-and-drop
 * is handled by Rust's window event. A path parameter here would let the
 * webview ask the backend to read any file, and would then need an
 * approved-path allowlist to close that hole again. */

/** Opens a multi-select dialog filtered to the formats the probe can verify.
 * Resolves how many files were queued — `0` when the user cancels, which is
 * not an error. Nothing starts transcribing; that is `importStart`. */
export const importPickFiles = () => invoke<number>("import_pick_files");
/** Begins the run. Snapshots the API key and language for its whole duration,
 * so a settings change mid-run is not picked up — cancel and start again.
 * Rejects with a sentence when no Sarvam key is stored. */
export const importStart = () => invoke<void>("import_start");
/** Stops the loop, the request on the wire, and the UI at once. Resolves how
 * many in-flight operations were aborted; `0` is normal, not an error. */
export const importCancel = () => invoke<number>("import_cancel");
/** Empties the queue, stopping whatever run it belonged to. */
export const importClear = () => invoke<void>("import_clear");
/** The current queue, in the same shape `import://progress` carries — for a
 * page that mounts halfway through a run. */
export const importStatus = () => invoke<ImportProgressPayload>("import_status");

export function formatBytes(bytes: number): string {
  if (bytes >= 1_000_000_000) return `${(bytes / 1_000_000_000).toFixed(1)} GB`;
  return `${Math.round(bytes / 1_000_000)} MB`;
}

/** Current updater state; the `update://state` event carries the same shape. */
export const updateStatus = () => invoke<UpdateStatePayload>("update_status");
/** A manual check. Never gated by `settings.updates.autoCheck`. */
export const updateCheck = () => invoke<UpdateStatePayload>("update_check");
/** Download, verify and hand off to the installer; progress arrives as events.
 * Resolves as soon as the work is queued. */
export const updateInstall = () => invoke<void>("update_install");

/** Who is signed in to Butterfly Labs. `email` is known only once a token has
 * been spent this run — the address is stored nowhere, so `cloudStatus()` is
 * what fetches it after a launch (see its command's doc). */
export interface CloudStatus {
  signedIn: boolean;
  email: string | null;
}

/** Opens the system browser at Google's consent screen and resolves as soon as
 * it has been handed the URL — NOT when the sign-in finishes. The rest of the
 * round trip lands minutes later on the `butterflylabs://` deep link and
 * announces itself with the `cloud-auth-changed` event, so a caller shows
 * "waiting for your browser" here and lets that event end it.
 *
 * Rejects with a sentence only when the browser could not be opened at all. */
export const cloudSignIn = () => invoke<void>("cloud_sign_in");
/** What a sign-out did on Supabase's side (`auth::session::SignOut`):
 * `ended` it there, signed out `hereOnly` because Supabase couldn't be
 * reached or didn't end the sign-in (an outage, or it refused this computer's
 * tokens), or found nothing signed in (`notSignedIn`, which is how cancelling
 * a sign-in still out at the browser ends). */
export type SignOutOutcome = "ended" | "hereOnly" | "notSignedIn";

/** Revokes the session and deletes the stored refresh token. The local half
 * happens even when the machine is offline, and the answer says whether
 * Supabase ended its side too; emits `cloud-auth-changed`. Never rejects. */
export const cloudSignOut = () => invoke<SignOutOutcome>("cloud_sign_out");

/** Shown after a sign-out that answered `hereOnly`. Supabase's local logout
 * ends only the session whose token asks, and this one's tokens are gone, so
 * only deleting the account removes that record. "Any record it has" because
 * a refused refresh cannot say whether one is left: after a reused refresh
 * token GoTrue keeps the session, but a session that is already gone is
 * refused too. */
export const SIGNED_OUT_HERE_ONLY =
  "Signed out on this computer, but Supabase couldn't be reached or didn't end this sign-in, so any record it has of the sign-in stays until you delete your Cloud account.";
/** Current sign-in. May make one network call — the first call after a launch
 * spends the stored refresh token to learn the address, and clears the
 * credential if the server has revoked it. Never rejects. */
export const cloudStatus = () => invoke<CloudStatus>("cloud_status");

/** This week's dictated words, counted by the relay. `weekStart` is the
 * Monday of the current ISO week. */
export interface CloudUsage {
  weekStart: string;
  words: number;
  limit: number;
}

/** Asks the relay for the week's count with the signed-in bearer. Rejects
 * whenever the number is unknown — signed out, offline, or a relay that
 * refused — so a caller shows a dash rather than a misleading zero. */
export const cloudUsage = () => invoke<CloudUsage>("cloud_usage");

/** Deletes the signed-in user's Cloud account: the relay's weekly count and
 * the Supabase account with everything it holds. Signs out here only once the
 * relay confirms, and emits `cloud-auth-changed`. Rejects with a sentence
 * otherwise, still signed in so it can be tried again, unless the sign-in
 * turned out to be revoked: then it has signed out and emitted the event too. */
export const cloudDeleteAccount = () => invoke<void>("cloud_delete_account");
