/**
 * One-way move of the old Scratchpad's `localStorage` notes into the SQLite
 * `notes` table.
 *
 * The Scratchpad page, which the Notes page replaces, kept everything the user
 * ever dictated into it in a single JSON blob under `bs-scratchpad-v1`, as
 * `{ id, text, updatedAt }`.
 * Those are real documents, so this runs once, on the first load of the Notes
 * page, and is built so that no interruption can lose one.
 *
 * ## The order of operations, and why it is that order
 *
 * 1. **Archive before creating anything.** The whole raw string is copied to
 *    `bs-scratchpad-v1.migrated` first. The key is renamed, never deleted, so
 *    if some later bug eats the imported rows the user's original text is
 *    still sitting in `localStorage` where it can be read back by hand.
 * 2. **Create notes oldest-first**, shrinking the source key after each one
 *    lands. The source key doubles as the work cursor: a crash halfway through
 *    leaves exactly the un-migrated tail behind, so the retry on the next load
 *    neither loses an entry nor duplicates one already written.
 * 3. **Remove the source key only when the tail is empty.** Its absence is the
 *    "done" marker, which is what makes the whole thing idempotent — a second
 *    call finds nothing and returns immediately.
 *
 * If a `create_note` fails (the DB thread never started, say) the source key
 * keeps the rest and the caller is told; nothing is renamed away, and the next
 * load tries again.
 *
 * ## Why there is an in-flight latch
 *
 * `+page.svelte` renders the active page from a `$derived`, so **every**
 * navigation to Notes mounts a fresh `Notes.svelte` and re-runs its `onMount`.
 * Nothing about the cursor protocol above survives two of these running at
 * once: both read the same source key, both walk the same list, and the two
 * loops advance the cursor in lockstep — the second call re-imports whatever
 * the first has not yet shifted off, so six entries become twelve notes. The
 * cursor is crash-safe, not concurrency-safe, and making it concurrency-safe
 * would need a lock `localStorage` does not have. So the module keeps the one
 * in-flight promise and hands the same one to every caller until it settles.
 */

import { createNote } from "$lib/api";

const SOURCE_KEY = "bs-scratchpad-v1";
/** The archive: the source key is renamed to this, never deleted. */
const ARCHIVE_KEY = "bs-scratchpad-v1.migrated";

/** The entry shape the Scratchpad page wrote. */
interface ScratchpadEntry {
  id: string;
  text: string;
  updatedAt: number;
}

export interface ScratchpadMigrationResult {
  /** Notes written to SQLite by this call. */
  migrated: number;
  /** Entries left behind because a write failed; retried on the next load. */
  failed: number;
  /**
   * Set when the migration could not even start, so the caller can say
   * something truthful instead of reporting a silent no-op.
   *
   * `"archive"` means `localStorage` refused the archive write — quota, or a
   * profile with storage locked down. Nothing was created, deliberately: a
   * note must never exist in SQLite without its original still recoverable
   * from `localStorage`. This state does not clear itself, so it is the one
   * the user has to be told about.
   */
  blocked: "archive" | null;
}

const NOTHING: ScratchpadMigrationResult = { migrated: 0, failed: 0, blocked: null };

/** Same validation the Scratchpad page applied on load, so nothing it would
 * have shown is dropped here and nothing it ignored is imported. */
function isEntry(v: unknown): v is ScratchpadEntry {
  return (
    typeof v === "object" &&
    v !== null &&
    typeof (v as ScratchpadEntry).id === "string" &&
    typeof (v as ScratchpadEntry).text === "string" &&
    typeof (v as ScratchpadEntry).updatedAt === "number"
  );
}

/** How many characters of the first line become the note's title. */
export const TITLE_MAX_CHARS = 80;

/**
 * The Scratchpad had no title field — it showed the first non-blank line — so
 * that line becomes the title and the whole text stays as the content. Nothing
 * is removed from the body: the title is a copy, not a cut.
 *
 * Truncation counts **code points**, not UTF-16 units, so an 80-character cut
 * can never land in the middle of a surrogate pair and leave half an emoji in
 * the list.
 */
export function scratchpadTitle(text: string): string {
  const first = text.split("\n").find((l) => l.trim().length > 0)?.trim() ?? "";
  const points = Array.from(first);
  if (points.length <= TITLE_MAX_CHARS) return first;
  return points.slice(0, TITLE_MAX_CHARS).join("").trimEnd();
}

function read(key: string): string | null {
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
}

function write(key: string, value: string): boolean {
  try {
    localStorage.setItem(key, value);
    return true;
  } catch {
    // Quota or a locked-down profile. The caller treats this as "stop", not
    // as "carry on and hope", because the cursor is what keeps the retry
    // honest.
    return false;
  }
}

/**
 * The Scratchpad tracked one timestamp per note and `isEntry` only checks that
 * it is a `number` — which `NaN`, `Infinity` and `1e300` all are. Handing one
 * of those to `create_note` makes the INSERT fail for that entry on this load
 * and on every load after it, which strands every *later* entry behind it
 * forever: the cursor never advances, and the page says "still waiting" at a
 * retry that can never succeed.
 *
 * `undefined` instead means "no override", so the row takes the column default
 * of now. A note that arrives with today's date is a small loss; a note that
 * can never arrive is not.
 */
function timestampOf(entry: ScratchpadEntry): number | undefined {
  return Number.isSafeInteger(entry.updatedAt) ? entry.updatedAt : undefined;
}

/**
 * The one migration in flight, or `null`.
 *
 * See the module doc: navigating to Notes twice mounts the page twice, and two
 * concurrent walks of the cursor duplicate every entry the first has not yet
 * shifted. Cleared in `finally` so a load that failed can still be retried by
 * the next navigation.
 */
let inflight: Promise<ScratchpadMigrationResult> | null = null;

/**
 * Migrate, if there is anything to migrate. Safe to call on every load, and
 * safe to call again while a previous call is still running — the second
 * caller is handed the first call's promise rather than starting a second
 * walk.
 */
export function migrateScratchpad(): Promise<ScratchpadMigrationResult> {
  if (inflight) return inflight;
  inflight = run().finally(() => {
    inflight = null;
  });
  return inflight;
}

async function run(): Promise<ScratchpadMigrationResult> {
  if (typeof localStorage === "undefined") return NOTHING;

  const raw = read(SOURCE_KEY);
  if (raw === null) return NOTHING; // migrated already, or never used

  let entries: ScratchpadEntry[] = [];
  try {
    const parsed: unknown = JSON.parse(raw);
    if (Array.isArray(parsed)) entries = parsed.filter(isEntry);
  } catch {
    // Unparseable. The archive below still keeps the bytes; there is nothing
    // to import from them, so fall through and clear the source key.
  }

  // Step 1: archive first, and only if there is no archive yet — a retry must
  // never overwrite the full original with the shrunken cursor. A refusal here
  // is reported rather than swallowed: nothing has been created, nothing will
  // be, and the condition does not clear on its own.
  if (read(ARCHIVE_KEY) === null && !write(ARCHIVE_KEY, raw)) {
    console.error("scratchpad migration blocked: localStorage refused the archive write");
    return { migrated: 0, failed: entries.length, blocked: "archive" };
  }

  // Oldest first, so note ids ascend with the dates they carry and the list's
  // `ORDER BY updated_at DESC, id DESC` tie-break agrees with itself. Entries
  // whose timestamp is unusable sort to the front, where they get the column
  // default; `timestampOf` says why.
  entries.sort((a, b) => (timestampOf(a) ?? 0) - (timestampOf(b) ?? 0));

  // Every entry travels, empty ones included: deciding a user's note is not
  // worth keeping is not this function's call to make.
  const remaining = [...entries];
  let migrated = 0;
  for (const entry of entries) {
    const at = timestampOf(entry);
    try {
      await createNote({
        title: scratchpadTitle(entry.text),
        content: entry.text,
        // The Scratchpad only ever tracked one timestamp, so it is both.
        createdAt: at,
        updatedAt: at,
      });
    } catch (e) {
      console.error("scratchpad migration stopped:", e);
      break;
    }
    migrated += 1;
    remaining.shift();
    // Step 2: advance the cursor. If this write fails the entry would be
    // imported twice on a retry, so stop here rather than run on.
    if (remaining.length > 0 && !write(SOURCE_KEY, JSON.stringify(remaining))) break;
  }

  // Step 3: the source key's absence is the done marker. Empty the cursor
  // *before* removing it: the loop above stops writing once `remaining` is
  // empty, so the key still holds the final entry, and a `removeItem` the
  // browser refuses would otherwise leave that one entry to be imported a
  // second time on the next load. Writing "[]" first makes the refusal
  // harmless — the retry parses an empty list and has genuinely no work.
  if (remaining.length === 0) {
    write(SOURCE_KEY, "[]");
    try {
      localStorage.removeItem(SOURCE_KEY);
    } catch {
      /* the "[]" above is what makes this safe to ignore */
    }
  }

  return { migrated, failed: remaining.length, blocked: null };
}
