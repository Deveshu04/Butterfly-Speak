/**
 * Autosave for the note editor.
 *
 * Changes are recorded per note and per field. A note's changes are written
 * together once typing on that note has paused for the quiet interval, and
 * only the fields that actually changed go into the write, so a title or body
 * save never carries a stale action result and the reverse never happens.
 * Writes for one note run one at a time, in the order they were started, so
 * an older value can never land after a newer one.
 */
import type { Note } from "$lib/api";

/** The open note as the editor holds it. */
export type EditorDraft = {
  noteId: number;
  title: string;
  content: string;
  polishedBody: string | null;
};

/** The three fields a person can type into. */
export type EditorField = "title" | "content" | "polishedBody";

/** Values for some of the editable fields, shaped for `update_note`. */
export type FieldValues = Partial<Record<EditorField, string>>;

/** The editor's starting point for `note`: its stored row, with anything
 * typed into it that no write has confirmed yet laid on top. */
export function editorStateOf(note: Note, typed: FieldValues = {}): EditorDraft {
  return {
    noteId: note.id,
    title: typed.title ?? note.title,
    content: typed.content ?? note.content,
    polishedBody: typed.polishedBody ?? note.polishedBody,
  };
}

export type AutosaveOptions = {
  /** How long typing on a note has to pause before its changes are written. */
  quietMs: number;
  /** Write one note's changes. Resolves false when the write failed. */
  write: (noteId: number, values: FieldValues) => Promise<boolean>;
  /** Called whenever `busy()` flips. */
  onBusyChange?: (busy: boolean) => void;
};

export type Autosave = {
  /** Record that `field` of note `noteId` now reads `value`. */
  change(noteId: number, field: EditorField, value: string): void;
  /** Write every note's waiting changes now, and resolve once every write
   * started so far has finished. */
  flushAll(): Promise<void>;
  /** Forget a note's unwritten changes without writing them. A write for it
   * that is queued but not yet started is skipped as well. */
  discard(noteId: number): void;
  /** What was typed into `noteId` that no write has confirmed yet. */
  unconfirmed(noteId: number): FieldValues;
  /** True while any change is unwritten: waiting for its quiet interval,
   * put back after a failed write, or queued or under way. */
  busy(): boolean;
};

/** A typed value and the order it was typed in, across all notes. */
type Typed = { value: string; order: number };

/** The fields of one note that still have to be written. `timer` is null
 * for fields put back after a failed write: those go out with the note's
 * next change or the next flush, not on a timer of their own. */
type Waiting = {
  fields: Set<EditorField>;
  timer: ReturnType<typeof setTimeout> | null;
  /** Bumped by every change, so a timer can tell it has been superseded. */
  generation: number;
};

export function createAutosave(options: AutosaveOptions): Autosave {
  let typedCount = 0;
  let writesPending = 0;
  let reportedBusy = false;

  /** Per note, the newest value of each field that no write has confirmed. */
  const newest = new Map<number, Map<EditorField, Typed>>();
  /** Per note, what still has to be written. */
  const waiting = new Map<number, Waiting>();
  /** Per note, the last write in its chain. */
  const chains = new Map<number, Promise<void>>();
  /** Per note, how many times it was discarded. A queued write compares it. */
  const discards = new Map<number, number>();

  function busy(): boolean {
    return writesPending > 0 || waiting.size > 0;
  }

  function report() {
    const now = busy();
    if (now === reportedBusy) return;
    reportedBusy = now;
    options.onBusyChange?.(now);
  }

  function waitingFor(noteId: number): Waiting {
    let w = waiting.get(noteId);
    if (!w) {
      w = { fields: new Set(), timer: null, generation: 0 };
      waiting.set(noteId, w);
    }
    return w;
  }

  function change(noteId: number, field: EditorField, value: string) {
    let fields = newest.get(noteId);
    if (!fields) {
      fields = new Map();
      newest.set(noteId, fields);
    }
    fields.set(field, { value, order: ++typedCount });

    const w = waitingFor(noteId);
    w.fields.add(field);
    w.generation += 1;
    if (w.timer !== null) clearTimeout(w.timer);
    const armedAt = w.generation;
    w.timer = setTimeout(() => {
      // Stale if a later change re-armed the timer or a flush already sent
      // these fields; either way the newer path does the write.
      if (waiting.get(noteId) !== w || w.generation !== armedAt) return;
      void send(noteId);
    }, options.quietMs);
    report();
  }

  /** Start writing one note's waiting fields, behind any write for the same
   * note that is still running. */
  function send(noteId: number): Promise<void> {
    const w = waiting.get(noteId);
    if (!w) return chains.get(noteId) ?? Promise.resolve();
    waiting.delete(noteId);
    if (w.timer !== null) clearTimeout(w.timer);

    const typed = newest.get(noteId);
    const batch = new Map<EditorField, Typed>();
    for (const field of w.fields) {
      const t = typed?.get(field);
      if (t) batch.set(field, t);
    }
    if (batch.size === 0) {
      report();
      return chains.get(noteId) ?? Promise.resolve();
    }

    const discardsAtSend = discards.get(noteId) ?? 0;
    writesPending += 1;
    report();

    const previous = chains.get(noteId) ?? Promise.resolve();
    const run = previous
      .then(async () => {
        if ((discards.get(noteId) ?? 0) !== discardsAtSend) return;
        const values: FieldValues = {};
        for (const [field, t] of batch) values[field] = t.value;
        let ok = false;
        try {
          ok = await options.write(noteId, values);
        } catch (e) {
          console.error("autosave write failed:", e);
        }
        settle(noteId, batch, ok, discardsAtSend);
      })
      .finally(() => {
        writesPending -= 1;
        if (chains.get(noteId) === run) chains.delete(noteId);
        report();
      });
    chains.set(noteId, run);
    return run;
  }

  /** Clear what a write confirmed, or put a failed field back in line. A
   * field typed again since the batch left is skipped both ways: its newer
   * value is already waiting or on its way. */
  function settle(noteId: number, batch: Map<EditorField, Typed>, ok: boolean, discardsAtSend: number) {
    if ((discards.get(noteId) ?? 0) !== discardsAtSend) return;
    const typed = newest.get(noteId);
    if (!typed) return;
    for (const [field, sent] of batch) {
      if (typed.get(field)?.order !== sent.order) continue;
      if (ok) typed.delete(field);
      else waitingFor(noteId).fields.add(field);
    }
    if (typed.size === 0) newest.delete(noteId);
  }

  async function flushAll(): Promise<void> {
    const started = [...waiting.keys()].map((noteId) => send(noteId));
    await Promise.all([...started, ...chains.values()]);
  }

  function discard(noteId: number) {
    const w = waiting.get(noteId);
    if (w?.timer != null) clearTimeout(w.timer);
    waiting.delete(noteId);
    newest.delete(noteId);
    discards.set(noteId, (discards.get(noteId) ?? 0) + 1);
    report();
  }

  function unconfirmed(noteId: number): FieldValues {
    const values: FieldValues = {};
    for (const [field, t] of newest.get(noteId) ?? []) values[field] = t.value;
    return values;
  }

  return { change, flushAll, discard, unconfirmed, busy };
}
