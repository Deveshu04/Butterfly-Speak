<script lang="ts">
  import { TauriEvent } from "@tauri-apps/api/event";
  import { getCurrentWindow } from "@tauri-apps/api/window";
  import { onDestroy, onMount, tick } from "svelte";
  import {
    createFolder,
    createNote,
    deleteFolder,
    deleteNote,
    exportNote,
    generateNoteTitle,
    getNote,
    listFolders,
    listNoteActions,
    listNotes,
    renameFolder,
    runNoteAction,
    searchNotes,
    updateNote,
    type Folder,
    type Note,
    type NoteAction,
    type NoteUpdate,
  } from "$lib/api";
  import { openRequest } from "$lib/openRequest.svelte";
  import {
    createAutosave,
    editorStateOf,
    type EditorDraft,
    type EditorField,
  } from "$lib/notes/autosave";
  import { migrateScratchpad } from "$lib/notes/scratchpadMigration";
  import { glyphFor } from "$lib/notes/glyphs";
  import ActionManager from "$lib/notes/ActionManager.svelte";
  import EmptyState from "$lib/components/EmptyState.svelte";
  import Icon from "$lib/components/Icon.svelte";
  import Dropdown from "$lib/components/Dropdown.svelte";

  /** `notes::PAGE_SIZE`. A short page that comes back as a full page means
   * there is nothing more — the same test History.svelte uses. */
  const PAGE_SIZE = 30;
  /** History.svelte's search cadence, so the two search boxes feel the same. */
  const SEARCH_DEBOUNCE_MS = 250;
  /** How long typing has to pause before the open note saves itself. Long
   * enough that writes land between sentences rather than between words;
   * closing, hiding and unloading save at once anyway, so this only bounds
   * what a crash could lose. */
  const AUTOSAVE_QUIET_MS = 1500;
  /** The folder picker's "no folder" option value. A sentinel rather than `""`
   * so an empty string can never be coerced into folder id 0. */
  const UNFILED = "unfiled";

  // ---- List state ---------------------------------------------------------
  let notes = $state<Note[]>([]);
  let query = $state("");
  let page = $state(0);
  let hasMore = $state(false);
  let loading = $state(false);
  let loadedOnce = $state(false);

  // ---- Folder state -------------------------------------------------------
  let folders = $state<Folder[]>([]);
  /**
   * Which folder the list is showing, in `notes::ListNotesArgs.folder`'s own three
   * states: `undefined` is every note, `null` is the unfiled ones, a number is
   * one folder. Spelled the same as the Rust side on purpose — "unfiled" is a
   * real destination here, not the absence of one, and collapsing the two into
   * a single nullable would lose that.
   */
  let folderScope = $state<number | null | undefined>(undefined);
  /** Sentences from `create_folder`/`rename_folder`, shown as they come. */
  let folderError = $state<string | null>(null);
  let renamingId = $state<number | null>(null);
  let renameValue = $state("");
  let creatingFolder = $state(false);
  let newFolderName = $state("");
  let confirmingFolder = $state<Folder | null>(null);

  let newFolderEl = $state<HTMLInputElement | null>(null);
  let renameEl = $state<HTMLInputElement | null>(null);

  // ---- Editor state -------------------------------------------------------
  let draft = $state<EditorDraft | null>(null);
  /** The folder picker's options: "Unfiled" first, then the folders in the
   * order the list shows them. Shaped for `components/Dropdown`, which every
   * other picker in the app uses; a native <select> would draw Windows' own
   * chrome in the middle of an app-styled toolbar. */
  let folderOptions = $derived([
    { value: UNFILED, label: "Unfiled" },
    ...folders.map((f) => ({ value: String(f.id), label: f.name })),
  ]);

  /** The open note's folder, kept beside the draft rather than inside it: the
   * draft holds only the fields a person types into, which are what autosave
   * writes. A move is one immediate write, not something to wait on a pause
   * for. */
  let draftFolderId = $state<number | null>(null);
  /** True while anything is queued or in flight, so the editor can say so. */
  let unsaved = $state(false);
  let inFlight = $state(0);
  let saveError = $state<string | null>(null);
  let confirming = $state<Note | null>(null);
  let migratedNotice = $state<string | null>(null);
  /** The migration's own problem slot. Deliberately not `saveError`: that one
   * is cleared by the next successful autosave, and "some of your Scratchpad
   * notes have not moved over yet" is a standing condition that must not
   * disappear because an unrelated keystroke saved cleanly. */
  let migrationError = $state<string | null>(null);

  let titleEl = $state<HTMLInputElement | null>(null);
  let contentEl = $state<HTMLTextAreaElement | null>(null);

  // ---- Note AI actions and auto-title -------------------------------------
  let actions = $state<NoteAction[]>([]);
  let actionMenuOpen = $state(false);
  let menuWrapEl = $state<HTMLSpanElement | null>(null);
  let managingActions = $state(false);
  /** `action:<id>` or `title` while one is running; `null` when idle. One
   * string rather than two booleans because only one may run at a time. The
   * runs are awaited, so the disabled state of the two buttons is the whole
   * guard. There is no way yet to cancel a run once it has started. */
  let busyWith = $state<string | null>(null);
  /** Why the last action or title didn't work. Shown, never swallowed. */
  let actionNotice = $state<string | null>(null);

  let searchTimer: ReturnType<typeof setTimeout> | undefined;
  let requestToken = 0;

  let searching = $derived(query.trim().length > 0);
  let saveLabel = $derived(unsaved || inFlight > 0 ? "Saving…" : "Saved");
  let scopeLabel = $derived(
    folderScope === undefined
      ? "All notes"
      : folderScope === null
        ? "Unfiled"
        : (folders.find((f) => f.id === folderScope)?.name ?? "All notes"),
  );

  // -------------------------------------------------------------------------
  // Autosave. Every edit is recorded under the note it was typed into; see
  // `notes/autosave.ts` for when and how the writes go out.
  // -------------------------------------------------------------------------

  const autosave = createAutosave({
    quietMs: AUTOSAVE_QUIET_MS,
    write: (noteId, values) => writeNote(noteId, values),
    onBusyChange: (busy) => (unsaved = busy),
  });

  /** Resolves false when the write failed. `saveError` is how a failed save
   * is reported; autosave uses the result only to keep the failed fields for
   * another try, but the folder picker needs it: a control that has already
   * moved to the new folder has to move back if the row did not. */
  async function writeNote(id: number, update: NoteUpdate): Promise<boolean> {
    inFlight += 1;
    try {
      await updateNote(id, update);
      saveError = null;
      applyLocally(id, update);
      return true;
    } catch (e) {
      console.error("note save failed:", e);
      // Never the note's text, and never the exception's own wording: the
      // point is that what is on screen is still there to copy out.
      saveError = "Couldn't save that note. Your text is still here — try again in a moment.";
      return false;
    } finally {
      inFlight -= 1;
    }
  }

  /** Keep the list row in step with a write that just landed. Deliberately
   * does not re-sort: rows must not jump under the cursor mid-edit. The list
   * re-orders on the next load, which `backToList` triggers. */
  function applyLocally(id: number, update: NoteUpdate) {
    const row = notes.find((n) => n.id === id);
    if (!row) return;
    if (update.title !== undefined) row.title = update.title;
    if (update.content !== undefined) row.content = update.content;
    if (update.polishedBody !== undefined) row.polishedBody = update.polishedBody;
    if (update.folderId !== undefined) row.folderId = update.folderId;
    row.updatedAt = Date.now();
  }

  // -------------------------------------------------------------------------
  // Editor.
  // -------------------------------------------------------------------------

  /**
   * Put `next` in the editor, or go back to the list when it is `null`.
   *
   * The incoming editor starts from `next`'s row, with anything typed into
   * that note which no write has confirmed yet laid on top, so a row read
   * before a save landed cannot bring back older text. Whatever the outgoing
   * note still owes is written under its own id. The switch itself does not
   * wait for that; the returned promise resolves once those writes are done.
   *
   * Picking the note that is already open changes nothing: what is on screen
   * is newer than any row read elsewhere.
   */
  function showInEditor(next: Note | null): Promise<void> {
    if (next && draft?.noteId === next.id) return Promise.resolve();
    actionMenuOpen = false;
    actionNotice = null;
    draft = next ? editorStateOf(next, autosave.unconfirmed(next.id)) : null;
    draftFolderId = next ? next.folderId : null;
    return autosave.flushAll();
  }

  /** The note each editor element was created for. The editors sit in a
   * block keyed on the open note's id, so an element never changes owner. */
  const editorOwners = new WeakMap<Element, number>();

  function ownedBy(node: HTMLElement, noteId: number) {
    editorOwners.set(node, noteId);
  }

  /**
   * Record one edit from one of the three editors.
   *
   * An event from an element created for a note other than the open one can
   * only have arrived after a switch — a late input or composition event, or
   * a paste that settled a frame late — and it is dropped, so it can never
   * change the note now on screen.
   */
  function takeEdit(field: EditorField, element: HTMLInputElement | HTMLTextAreaElement) {
    const owner = editorOwners.get(element);
    if (!draft || owner !== draft.noteId) return;
    draft[field] = element.value;
    autosave.change(owner, field, element.value);
  }

  async function openNote(note: Note) {
    void showInEditor(note);
    await tick();
    contentEl?.focus();
  }

  async function backToList() {
    await showInEditor(null);
    reload();
  }

  async function startNewNote() {
    await autosave.flushAll();
    try {
      // A new note lands in the folder you are looking at. Nothing seeds a
      // default folder, so "All notes" and "Unfiled" both mean unfiled.
      const id = await createNote({ folderId: folderScope ?? null });
      const note = await getNote(id);
      if (!note) return;
      notes = [note, ...notes];
      void showInEditor(note);
      // Only a folder has a count on screen to move; an unfiled note has none.
      if (typeof folderScope === "number") void refreshFolders();
      await tick();
      titleEl?.focus();
    } catch (e) {
      console.error("couldn't create a note:", e);
      saveError = "Couldn't start a new note. Try again in a moment.";
    }
  }

  /**
   * Move the open note to another folder — or out of every folder.
   *
   * `updateNote({ folderId })` is the whole operation; there is no separate
   * move command. The control moves first and moves back if the write fails,
   * so it never shows a folder the row is not in.
   */
  async function moveDraftToFolder(target: number | null) {
    if (!draft || target === draftFolderId) return;
    const id = draft.noteId;
    const previous = draftFolderId;
    draftFolderId = target;
    const ok = await writeNote(id, { folderId: target });
    if (!ok) {
      draftFolderId = previous;
      return;
    }
    await refreshFolders();
  }

  // -------------------------------------------------------------------------
  // Folders. Flat: no nesting and no reordering. `sort_order` is written once
  // at create and never updated, so there is no reorder control to build.
  // -------------------------------------------------------------------------

  async function refreshFolders() {
    try {
      folders = await listFolders();
    } catch (e) {
      console.error("folder list failed:", e);
    }
  }

  /**
   * One folder write at a time.
   *
   * Both inline editors commit on Enter *and* on blur, which is what makes
   * them feel like a rename in a file manager — and it means a click that
   * lands while the first write is still in flight submits the same buffer
   * twice. For a create that is a second `create_folder` with the same name,
   * which succeeds and then reports "You already have a folder called that."
   * over the folder it just made. Not `$state`: nothing on screen reads it.
   */
  let folderBusy = false;

  /** `create_folder`, `rename_folder` and `delete_folder` return
   * `Result<_, String>`, and their strings are written to be shown as-is
   * ("You already have a folder called that. Pick another name."). Anything
   * that is not one of those — an ACL rejection, a dead IPC — gets our own
   * sentence instead of an internal one. */
  function folderMessage(e: unknown, fallback: string): string {
    return typeof e === "string" && e.trim() ? e : fallback;
  }

  /**
   * The exact buffer a create or a rename was last refused for.
   *
   * Both editors commit on blur, and a refusal leaves the editor on screen
   * with its sentence — which is the point, so the typed name is not thrown
   * away. Without this, the very next click anywhere (including a rail row,
   * which would also select it) blurs the input and fires the same doomed
   * call again, so "You already have a folder called that." reappears for a
   * name the user never re-submitted.
   *
   * Keyed on the buffer rather than on `folderError` being set, so that
   * editing one character re-arms the blur commit on its own — and so the two
   * editors, which can be open at once, cannot silence each other. Enter is
   * always an explicit retry and is never suppressed.
   */
  let refusedNewFolderName: string | null = null;
  let refusedRenameName: string | null = null;

  function selectScope(next: number | null | undefined) {
    folderScope = next;
    // Search is deliberately not folder-scoped (`search_notes` indexes every
    // note), so a live query and a folder selection would contradict each
    // other on screen. Choosing a folder is the more specific act; it wins.
    query = "";
    if (searchTimer) clearTimeout(searchTimer);
    page = 0;
    runList(true);
  }

  async function startNewFolder() {
    folderError = null;
    creatingFolder = true;
    newFolderName = "";
    await tick();
    newFolderEl?.focus();
  }

  function cancelNewFolder() {
    creatingFolder = false;
    newFolderName = "";
    folderError = null;
    refusedNewFolderName = null;
  }

  async function submitNewFolder(viaBlur = false) {
    if (folderBusy) return;
    const name = newFolderName.trim();
    if (!name) {
      cancelNewFolder();
      return;
    }
    if (viaBlur && name === refusedNewFolderName) return;
    folderError = null;
    folderBusy = true;
    try {
      const folder = await createFolder(name);
      cancelNewFolder();
      await refreshFolders();
      selectScope(folder.id);
    } catch (e) {
      refusedNewFolderName = name;
      folderError = folderMessage(e, "Couldn't create that folder. Try again in a moment.");
    } finally {
      folderBusy = false;
    }
  }

  async function startRename(folder: Folder) {
    folderError = null;
    renamingId = folder.id;
    renameValue = folder.name;
    await tick();
    renameEl?.select();
  }

  function cancelRename() {
    renamingId = null;
    renameValue = "";
    folderError = null;
    refusedRenameName = null;
  }

  async function submitRename(viaBlur = false) {
    if (folderBusy) return;
    const id = renamingId;
    if (id === null) return;
    const name = renameValue.trim();
    const current = folders.find((f) => f.id === id);
    if (!name || name === current?.name) {
      cancelRename();
      return;
    }
    if (viaBlur && name === refusedRenameName) return;
    folderError = null;
    folderBusy = true;
    try {
      await renameFolder(id, name);
      cancelRename();
      await refreshFolders();
    } catch (e) {
      refusedRenameName = name;
      folderError = folderMessage(e, "Couldn't rename that folder. Try again in a moment.");
    } finally {
      folderBusy = false;
    }
  }

  /** The count comes off the row `list_folders` already returned, which is why
   * the confirmation can name it without a second query. */
  function askDeleteFolder(folder: Folder) {
    folderError = null;
    confirmingFolder = folder;
  }

  async function confirmDeleteFolder() {
    const folder = confirmingFolder;
    if (!folder || folderBusy) return;
    confirmingFolder = null;
    folderBusy = true;
    try {
      const goneIds = await deleteFolder(folder.id);
      // Settling a queued save for a row that no longer exists is a write
      // racing a DELETE. (Today the rail is only on screen in list mode, where
      // `backToList` has already flushed — but that is a property of the
      // layout, not of this function.)
      for (const id of goneIds) autosave.discard(id);
      const gone = new Set(goneIds);
      notes = notes.filter((n) => !gone.has(n.id));
      if (draft && gone.has(draft.noteId)) draft = null;
      await refreshFolders();
      // Only fall back to "All notes" if the folder that went was the one
      // being viewed. `selectScope` clears the search box — that is right when
      // the scope moves under the user, and wrong when it did not: deleting an
      // unrelated folder would otherwise wipe a live query for no reason. The
      // list still has to be re-read either way, because the cascade can have
      // taken rows out of the view that is on screen.
      if (folderScope === folder.id) selectScope(undefined);
      else reload();
    } catch (e) {
      folderError = folderMessage(e, "Couldn't delete that folder. Try again in a moment.");
    } finally {
      folderBusy = false;
    }
  }

  // -------------------------------------------------------------------------
  // Delete, always behind a confirmation. These are hard deletes with no
  // trash to recover from, so the question is the only thing standing between
  // a stray click and a lost document.
  // -------------------------------------------------------------------------
  function askDelete(note: Note) {
    confirming = note;
  }

  async function confirmDelete() {
    const note = confirming;
    if (!note) return;
    confirming = null;
    autosave.discard(note.id);
    try {
      const gone = await deleteNote(note.id);
      if (!gone) return;
      notes = notes.filter((n) => n.id !== note.id);
      if (draft?.noteId === note.id) draft = null;
      if (note.folderId !== null) void refreshFolders();
    } catch (e) {
      console.error("note delete failed:", e);
      saveError = "Couldn't delete that note. Try again in a moment.";
    }
  }

  // -------------------------------------------------------------------------
  // List + search — History.svelte's shape: a page counter for the list, a
  // debounced token-guarded search, and one `reload` that knows which is live.
  // -------------------------------------------------------------------------
  async function runSearch() {
    const token = ++requestToken;
    loading = true;
    try {
      const rows = await searchNotes(query.trim());
      if (token !== requestToken) return; // a newer request already landed
      notes = rows;
      hasMore = false;
    } catch (e) {
      console.error("note search failed:", e);
    } finally {
      if (token === requestToken) loading = false;
      loadedOnce = true;
    }
  }

  async function runList(reset: boolean) {
    const token = ++requestToken;
    const targetPage = reset ? 0 : page + 1;
    loading = true;
    try {
      // `folder` omitted lists every note, `null` lists the unfiled ones — the
      // `Option<Option<i64>>` distinction, which only survives if the key is
      // absent rather than `undefined`.
      const rows = await listNotes(
        folderScope === undefined ? { page: targetPage } : { folder: folderScope, page: targetPage },
      );
      if (token !== requestToken) return;
      notes = reset ? rows : [...notes, ...rows];
      page = targetPage;
      hasMore = rows.length === PAGE_SIZE;
    } catch (e) {
      console.error("note list failed:", e);
    } finally {
      if (token === requestToken) loading = false;
      loadedOnce = true;
    }
  }

  function reload() {
    if (searching) {
      runSearch();
    } else {
      runList(true);
    }
  }

  function loadMore() {
    if (!searching && !loading) runList(false);
  }

  function onQueryInput() {
    if (searchTimer) clearTimeout(searchTimer);
    searchTimer = setTimeout(reload, SEARCH_DEBOUNCE_MS);
  }

  // -------------------------------------------------------------------------
  // Presentation helpers.
  // -------------------------------------------------------------------------

  /** `notes.updated_at` is epoch **milliseconds**, so unlike History's
   * `datetime('now')` strings it needs no UTC fixup. */
  function relativeTime(at: number): string {
    const delta = Date.now() - at;
    if (delta < 60_000) return "just now";
    if (delta < 3_600_000) return `${Math.floor(delta / 60_000)}m ago`;
    if (delta < 86_400_000) return `${Math.floor(delta / 3_600_000)}h ago`;
    return new Date(at).toLocaleDateString(undefined, { month: "short", day: "numeric" });
  }

  function preview(note: Note): string {
    const body = note.content.trim();
    if (!body) return "";
    const line = body.split("\n").find((l) => l.trim().length > 0)?.trim() ?? "";
    // The first line is usually the title as well; show the one after it.
    if (line === note.title.trim()) {
      const rest = body.slice(body.indexOf(line) + line.length).trim();
      return rest.split("\n").find((l) => l.trim().length > 0)?.trim() ?? "";
    }
    return line;
  }

  function wordCount(text: string): number {
    return text.split(/\s+/).filter(Boolean).length;
  }

  /** Null for an unfiled note, and null for a folder that is not in the rail
   * yet — the chip is a hint, not a source of truth. */
  function folderNameOf(id: number | null): string | null {
    if (id === null) return null;
    return folders.find((f) => f.id === id)?.name ?? null;
  }

  // -------------------------------------------------------------------------
  // Lifecycle.
  // -------------------------------------------------------------------------

  /** Hiding to the tray is not a page unload: `lib.rs`'s `CloseRequested`
   * handler calls `prevent_close()` and hides the window, so the webview lives
   * on and `beforeunload` never fires. Tauri still emits the close request to
   * this window, which is the hook that makes the last second of typing
   * survive the X button. */
  function onWindowHidden() {
    if (document.hidden) void autosave.flushAll();
  }

  function onPageHide() {
    // Best effort only: an actual teardown (tray → Quit, a dev reload) will not
    // wait for an `invoke` to come back. It costs nothing and covers the
    // reload case, where it does complete.
    void autosave.flushAll();
  }

  // -------------------------------------------------------------------------
  // Note AI actions and auto-title.
  //
  // Both write every waiting change FIRST. The model reads `content` from the
  // database, so an edit still waiting for its pause would mean it works on
  // the previous version — and, worse, stores a `polished_from_hash`
  // of text the user has already changed, which is the one thing that hash
  // exists to notice.
  // -------------------------------------------------------------------------

  async function loadActions() {
    try {
      actions = await listNoteActions();
    } catch (e) {
      console.error("couldn't load note actions:", e);
    }
  }

  /**
   * Run an action, then adopt exactly the row the Rust side stored.
   *
   * `run_note_action` writes `polished_body`, `polish_prompt` and
   * `polished_from_hash` together and answers with what it wrote, so
   * the draft takes that value directly instead of going through autosave,
   * which would schedule a second write on top of the one that just landed.
   */
  async function runAction(action: NoteAction) {
    const open = draft;
    if (!open || busyWith) return;
    actionMenuOpen = false;
    actionNotice = null;
    busyWith = `action:${action.id}`;
    try {
      await autosave.flushAll();
      const updated = await runNoteAction(open.noteId, action.id);
      // The editor may have moved to another note while the model answered.
      const current = draft;
      if (current && current.noteId === updated.id) {
        draft = { ...current, polishedBody: updated.polishedBody };
      }
      applyLocally(updated.id, { polishedBody: updated.polishedBody });
    } catch (e) {
      // The Rust side's messages are written to be shown as-is, and carry no
      // endpoint URL — see `notes::actions::enhance`.
      actionNotice = `${e}`;
    } finally {
      busyWith = null;
    }
  }

  /**
   * Ask the model for a title.
   *
   * A failure is a notice. That is the whole point of the command returning a
   * `Result`: a broken title model must never look like a note that simply
   * kept its placeholder, with nobody told why. The stored title is adopted
   * the same way an action's result is.
   */
  async function autoTitle() {
    const open = draft;
    if (!open || busyWith) return;
    actionMenuOpen = false;
    actionNotice = null;
    busyWith = "title";
    try {
      await autosave.flushAll();
      const title = await generateNoteTitle(open.noteId);
      const current = draft;
      if (current && current.noteId === open.noteId) {
        draft = { ...current, title };
      }
      applyLocally(open.noteId, { title });
    } catch (e) {
      actionNotice = `${e}`;
    } finally {
      busyWith = null;
    }
  }

  /**
   * Put the note back to just what was written.
   *
   * The columns are nullable and `NoteUpdate` distinguishes "absent" from an
   * explicit `null`, so this clears them rather than storing empty strings.
   * `record_run` writes the enhancement as three columns together —
   * `polishedBody`, `polishPrompt`, `polishedFromHash` — so
   * discarding clears the same three: leaving the prompt and the fingerprint
   * behind would describe an enhancement that is gone.
   *
   * Writes everything waiting first rather than discarding it: discarding
   * drops every unwritten field of the note, including a title or body edit
   * still waiting for its pause. Writing first and then clearing costs one
   * extra write and loses nothing. The clear itself does not go through
   * autosave.
   */
  async function discardEnhanced(noteId: number) {
    const current = draft;
    if (!current || current.noteId !== noteId) return;
    await autosave.flushAll();
    const after = draft;
    if (after && after.noteId === noteId) {
      draft = { ...after, polishedBody: null };
    }
    await writeNote(noteId, {
      polishedBody: null,
      polishPrompt: null,
      polishedFromHash: null,
    });
  }

  /**
   * Save the open note to a file the user picks. The export reads the stored
   * note, so everything waiting is written first. Both formats hold the
   * enhanced body when there is one; see `exportNote`. Cancelling the dialog
   * is not a failure and says nothing.
   */
  async function exportDraft(format: "md" | "txt") {
    const open = draft;
    if (!open || busyWith) return;
    actionNotice = null;
    try {
      await autosave.flushAll();
      await exportNote(open.noteId, format);
    } catch (e) {
      // The Rust side's messages are written to be shown as-is.
      actionNotice = `${e}`;
    }
  }

  /** A press anywhere outside the Enhance menu closes it, as a Dropdown's
   * panel closes. */
  function onDocPointerDown(e: PointerEvent) {
    if (actionMenuOpen && menuWrapEl && !menuWrapEl.contains(e.target as Node)) {
      actionMenuOpen = false;
    }
  }

  function onKeydown(e: KeyboardEvent) {
    if (e.key !== "Escape") return;
    if (confirming) {
      e.preventDefault();
      confirming = null;
    } else if (confirmingFolder) {
      e.preventDefault();
      confirmingFolder = null;
    } else if (actionMenuOpen) {
      e.preventDefault();
      actionMenuOpen = false;
    }
  }

  /**
   * A note or a folder picked in the Ctrl+K palette lands here.
   *
   * An `$effect` rather than a read in `onMount`, because both arrivals have
   * to work: the palette can be opened while this page is already on screen
   * (`onMount` will never run again), or from anywhere else, in which case the
   * shell's navigation is what creates this component and the effect runs on
   * mount. Draining the slot re-runs the effect once, which then finds nothing
   * and stops.
   */
  $effect(() => {
    const request = openRequest.take("note", "folder");
    if (!request) return;
    if (request.kind === "folder") {
      // The rail lives in the list view, so a folder picked while the editor
      // is open has to close it first — otherwise the selection lands behind a
      // full-page editor and looks like nothing happened. Going through the
      // transition is also what settles the open note's last keystrokes.
      const target = request.folderId;
      void showInEditor(null).then(() => selectScope(target));
    } else {
      void openNote(request.note);
    }
  });

  onMount(() => {
    let unlistenClose: (() => void) | undefined;
    // Deliberately a plain `listen`, not `window.onCloseRequested`. That
    // helper's contract is "the handler may veto the close", so when a handler
    // does not call `preventDefault()` it goes on to call `window.destroy()`
    // itself (as of @tauri-apps/api 2.11.1). This app has already decided
    // what a close means, in Rust, and it is *not* destroy:
    // `lib.rs` prevents it and hides to the tray. Riding on the helper would
    // either tear the window down behind that decision's back or — since
    // `core:window:allow-destroy` is not granted to this window — throw an ACL
    // rejection on every close. This listener only wants to be told.
    getCurrentWindow()
      .listen(TauriEvent.WINDOW_CLOSE_REQUESTED, () => {
        void autosave.flushAll();
      })
      .then((u) => (unlistenClose = u))
      .catch((e) => console.error("couldn't hook the window close for autosave:", e));

    document.addEventListener("visibilitychange", onWindowHidden);
    window.addEventListener("pagehide", onPageHide);

    void (async () => {
      // Held true across the migration as well as the list read. A migration
      // with a few dozen notes in it is a round-trip per note, and without
      // this the page renders its "No notes yet" empty state over a store
      // that is in the middle of being filled.
      loading = true;
      const { migrated, failed, blocked } = await migrateScratchpad();
      if (migrated > 0) {
        migratedNotice = `Moved ${migrated} ${migrated === 1 ? "note" : "notes"} over from your Scratchpad.`;
      }
      if (blocked === "archive") {
        migrationError =
          "Your Scratchpad notes haven't moved over yet — this device's browser storage refused to keep a backup copy first, and nothing is imported without one. They are still safe in the Scratchpad's storage.";
      } else if (failed > 0) {
        migrationError = `${failed} Scratchpad ${failed === 1 ? "note is" : "notes are"} still waiting to move over — they're safe, and this will retry.`;
      }
      await refreshFolders();
      reload();
    })();

    // Seeds the shipped actions on a fresh install — `list_note_actions` is
    // the first-use that fills the table (see `notes::actions`).
    void loadActions();

    return () => {
      unlistenClose?.();
      document.removeEventListener("visibilitychange", onWindowHidden);
      window.removeEventListener("pagehide", onPageHide);
    };
  });

  onDestroy(() => {
    if (searchTimer) clearTimeout(searchTimer);
    void autosave.flushAll();
  });
</script>

<svelte:window onkeydown={onKeydown} />
<svelte:document onpointerdowncapture={onDocPointerDown} />

<div class="page notes-page">
  {#if draft}
    {@const noteId = draft.noteId}
    <div class="editor">
      <div class="editor-bar">
        <button class="ghost" onclick={backToList}>&larr; {scopeLabel}</button>
        <div class="editor-bar-right">
          <Dropdown
            options={folderOptions}
            value={draftFolderId === null ? UNFILED : String(draftFolderId)}
            onchange={(v) => moveDraftToFolder(v === UNFILED ? null : Number(v))}
            ariaLabel="Folder"
            compact
          />
          <button
            class="ghost tool"
            onclick={autoTitle}
            disabled={busyWith !== null}
            title="Let the model name this note"
          >
            <Icon name="type" size={14} stroke={1.7} />
            {busyWith === "title" ? "Titling…" : "Auto-title"}
          </button>
          <span class="menu-wrap" bind:this={menuWrapEl}>
            <button
              class="ghost tool"
              aria-haspopup="menu"
              aria-expanded={actionMenuOpen}
              disabled={busyWith !== null}
              onclick={() => (actionMenuOpen = !actionMenuOpen)}
            >
              <Icon name="sparkles" size={14} stroke={1.7} />
              {busyWith?.startsWith("action:") ? "Working…" : "Enhance"}
            </button>
            {#if actionMenuOpen}
              <div class="action-menu" role="menu">
                {#each actions as action (action.id)}
                  <button class="menu-item" role="menuitem" onclick={() => runAction(action)}>
                    <!-- An unknown stored glyph draws the default rather than an empty box. -->
                    <Icon name={glyphFor(action.glyph)} size={15} stroke={1.6} />
                    <span class="menu-text">
                      <span class="menu-name">{action.label}</span>
                      {#if action.summary}
                        <span class="menu-desc">{action.summary}</span>
                      {/if}
                    </span>
                  </button>
                {/each}
                <button
                  class="menu-item manage"
                  role="menuitem"
                  onclick={() => {
                    actionMenuOpen = false;
                    managingActions = true;
                  }}
                >
                  <Icon name="sliders" size={15} stroke={1.6} />
                  <span class="menu-text"><span class="menu-name">Manage actions…</span></span>
                </button>
              </div>
            {/if}
          </span>
          <button
            class="ghost tool"
            onclick={() => exportDraft("md")}
            disabled={busyWith !== null}
            title="Save this note as a Markdown file"
          >
            Export .md
          </button>
          <button
            class="ghost tool"
            onclick={() => exportDraft("txt")}
            disabled={busyWith !== null}
            title="Save this note as a plain text file"
          >
            Export .txt
          </button>
          <span class="save-state" class:busy={unsaved || inFlight > 0}>
            {saveLabel} · {wordCount(draft.content)}
            {wordCount(draft.content) === 1 ? "word" : "words"}
          </span>
        </div>
      </div>

      {#if saveError}
        <p class="save-error">{saveError}</p>
      {/if}
      {#if actionNotice}
        <p class="save-error" role="alert">{actionNotice}</p>
      {/if}

      <!--
        The editors are keyed on the open note, so every note gets elements of
        its own and `takeEdit` can tell which note an input event was typed
        into.

        Locked while an action or auto-title runs (a few seconds, up to 60s on
        a long note). The run stores its result and the draft adopts it, so a
        keystroke landing in that window would be recorded against the old
        text and written over the result. Read-only for the whole window closes
        that race, the same way `disabled` closes it for the two buttons; each
        run writes everything waiting before it starts.
      -->
      {#key noteId}
        <input
          bind:this={titleEl}
          use:ownedBy={noteId}
          class="title-input"
          type="text"
          placeholder="Untitled"
          aria-label="Note title"
          spellcheck="false"
          readonly={busyWith !== null}
          value={draft.title}
          oninput={(e) => takeEdit("title", e.currentTarget)}
        />
        <textarea
          bind:this={contentEl}
          use:ownedBy={noteId}
          class="content-input"
          placeholder="Dictate or type your note…"
          aria-label="Note"
          spellcheck="false"
          readonly={busyWith !== null}
          value={draft.content}
          oninput={(e) => takeEdit("content", e.currentTarget)}
        ></textarea>

        <!--
          The enhanced pane. An action's result is stored BESIDE what was
          written, never over it, so this is a second buffer rather than a
          replacement, and it is editable: its edits autosave like the title
          and body, in their own field.
        -->
        {#if draft.polishedBody !== null}
          <div class="enhanced">
            <div class="enhanced-head">
              <span class="enhanced-label">
                <Icon name="sparkles" size={13} stroke={1.7} />
                Enhanced
              </span>
              <button class="ghost tool" onclick={() => discardEnhanced(noteId)}>Discard</button>
            </div>
            <!--
              Locked for the same window and the same reason: an edit here
              while `runAction` is answering would be written over the result
              it just stored.
            -->
            <textarea
              use:ownedBy={noteId}
              class="content-input enhanced-input"
              aria-label="Enhanced note"
              spellcheck="false"
              readonly={busyWith !== null}
              value={draft.polishedBody}
              oninput={(e) => takeEdit("polishedBody", e.currentTarget)}
            ></textarea>
          </div>
        {/if}
      {/key}

      {#if managingActions}
        <div class="manager-panel">
          <ActionManager onchange={loadActions} onclose={() => (managingActions = false)} />
        </div>
      {/if}
    </div>
  {:else}
    <h1 class="page-title">Notes</h1>
    <p class="page-desc">
      Longer pieces you dictate into and come back to — searchable, and kept on this device.
    </p>

    {#if migratedNotice}
      <div class="notice">
        <Icon name="check" size={16} stroke={2} />
        <span>{migratedNotice}</span>
        <button class="notice-close" aria-label="Dismiss" onclick={() => (migratedNotice = null)}>
          <Icon name="close" size={13} stroke={2} />
        </button>
      </div>
    {/if}

    {#if migrationError}
      <div class="notice caution">
        <Icon name="help" size={16} stroke={1.8} />
        <span>{migrationError}</span>
      </div>
    {/if}

    {#if saveError}
      <p class="save-error">{saveError}</p>
    {/if}

    <div class="workspace">
      <!-- The folder rail. Flat by construction — see the folder block in the
           script for why there is no nesting and no drag-to-reorder. -->
      <aside class="rail" aria-label="Folders">
        <button
          class="rail-row"
          class:selected={folderScope === undefined}
          onclick={() => selectScope(undefined)}
        >
          <Icon name="note" size={14} stroke={1.6} />
          <span class="rail-name">All notes</span>
        </button>
        <button
          class="rail-row"
          class:selected={folderScope === null}
          onclick={() => selectScope(null)}
        >
          <Icon name="file" size={14} stroke={1.6} />
          <span class="rail-name">Unfiled</span>
        </button>

        <div class="rail-sep"></div>

        {#each folders as folder (folder.id)}
          {#if renamingId === folder.id}
            <div class="rail-edit">
              <Icon name="folder" size={14} stroke={1.6} />
              <input
                bind:this={renameEl}
                bind:value={renameValue}
                type="text"
                aria-label="Folder name"
                spellcheck="false"
                onkeydown={(e) => {
                  if (e.key === "Enter") {
                    e.preventDefault();
                    submitRename();
                  } else if (e.key === "Escape") {
                    e.preventDefault();
                    cancelRename();
                  }
                }}
                onblur={() => submitRename(true)}
              />
            </div>
          {:else}
            <div class="rail-item" class:selected={folderScope === folder.id}>
              <button class="rail-row" onclick={() => selectScope(folder.id)}>
                <Icon name="folder" size={14} stroke={1.6} />
                <span class="rail-name">{folder.name}</span>
                <span class="rail-count">{folder.noteCount}</span>
              </button>
              <div class="rail-actions">
                <button
                  class="icon-btn"
                  aria-label="Rename folder"
                  title="Rename"
                  onclick={() => startRename(folder)}
                >
                  <Icon name="pencil" size={13} stroke={1.7} />
                </button>
                <button
                  class="icon-btn danger"
                  aria-label="Delete folder"
                  title="Delete"
                  onclick={() => askDeleteFolder(folder)}
                >
                  <Icon name="trash" size={13} stroke={1.6} />
                </button>
              </div>
            </div>
          {/if}
        {/each}

        {#if creatingFolder}
          <div class="rail-edit">
            <Icon name="folder" size={14} stroke={1.6} />
            <input
              bind:this={newFolderEl}
              bind:value={newFolderName}
              type="text"
              placeholder="Folder name"
              aria-label="New folder name"
              spellcheck="false"
              onkeydown={(e) => {
                if (e.key === "Enter") {
                  e.preventDefault();
                  submitNewFolder();
                } else if (e.key === "Escape") {
                  e.preventDefault();
                  cancelNewFolder();
                }
              }}
              onblur={() => submitNewFolder(true)}
            />
          </div>
        {:else}
          <button class="rail-add" onclick={startNewFolder}>
            <Icon name="plus" size={13} stroke={2} />
            <span>New folder</span>
          </button>
        {/if}

        {#if folderError}
          <p class="rail-error">{folderError}</p>
        {/if}
      </aside>

      <div class="list-col">
        <div class="toolbar">
          <div class="search-box">
            <Icon name="search" size={16} stroke={1.7} />
            <input
              type="text"
              placeholder="Search your notes…"
              aria-label="Search notes"
              bind:value={query}
              oninput={onQueryInput}
            />
            {#if query}
              <button
                class="clear-search"
                aria-label="Clear search"
                onclick={() => {
                  query = "";
                  reload();
                }}
              >
                <Icon name="close" size={13} stroke={2} />
              </button>
            {/if}
          </div>
          <button class="new-note" onclick={startNewNote}>
            <Icon name="plus" size={15} stroke={2} />
            <span>New note</span>
          </button>
        </div>

        {#if searching && folderScope !== undefined}
          <!-- `search_notes` has no folder filter: it indexes every note. Say
               so rather than let the rail imply a scope the results don't have. -->
          <p class="scope-note">Search looks in every folder.</p>
        {/if}

        {#if !loadedOnce && loading}
          <p class="loading-line">Loading…</p>
        {:else if notes.length === 0}
          {#if searching}
            <EmptyState
              icon="search"
              title="No matches"
              body={`Nothing in your notes matches "${query.trim()}".`}
            />
          {:else if folderScope === undefined}
            <EmptyState
              icon="note"
              title="No notes yet"
              body="Capture an idea, a task, or a thought without leaving what you're doing."
            >
              {#snippet action()}
                <button onclick={startNewNote}>Start a new note</button>
              {/snippet}
            </EmptyState>
          {:else}
            <EmptyState
              icon="folder"
              title={folderScope === null ? "Nothing unfiled" : `"${scopeLabel}" is empty`}
              body="Notes you start here land in this folder, and you can move any note into it from its editor."
            >
              {#snippet action()}
                <button onclick={startNewNote}>Start a new note</button>
              {/snippet}
            </EmptyState>
          {/if}
        {:else}
          <ul class="note-list">
            {#each notes as note (note.id)}
              <li>
                <button class="row" onclick={() => openNote(note)}>
                  <span class="row-main">
                    <span class="row-title" class:untitled={!note.title.trim()}>
                      {note.title.trim() || "Untitled"}
                    </span>
                    {#if preview(note)}
                      <span class="row-preview">{preview(note)}</span>
                    {/if}
                  </span>
                  {#if folderScope === undefined && folderNameOf(note.folderId)}
                    <span class="row-folder">{folderNameOf(note.folderId)}</span>
                  {/if}
                  <span class="row-time">{relativeTime(note.updatedAt)}</span>
                </button>
                <div class="row-actions">
                  <button
                    class="icon-btn danger"
                    aria-label="Delete note"
                    title="Delete"
                    onclick={() => askDelete(note)}
                  >
                    <Icon name="trash" size={15} stroke={1.6} />
                  </button>
                </div>
              </li>
            {/each}
          </ul>

          {#if hasMore}
            <button class="load-more" disabled={loading} onclick={loadMore}>
              {loading ? "Loading…" : "Load more"}
            </button>
          {/if}
        {/if}
      </div>
    </div>
  {/if}
</div>

{#if confirming}
  <div
    class="scrim"
    role="presentation"
    onclick={(e) => {
      if (e.target === e.currentTarget) confirming = null;
    }}
  >
    <div class="confirm" role="dialog" aria-modal="true" aria-labelledby="delete-note-title">
      <p id="delete-note-title" class="confirm-title">
        Delete "{confirming.title.trim() || "Untitled"}"?
      </p>
      <p class="confirm-body">
        The note and everything in it goes for good. There's no trash to fish it back out of.
      </p>
      <div class="confirm-actions">
        <button class="btn-secondary" onclick={() => (confirming = null)}>Cancel</button>
        <button class="btn-danger" onclick={confirmDelete}>Delete note</button>
      </div>
    </div>
  </div>
{/if}

{#if confirmingFolder}
  {@const count = confirmingFolder.noteCount}
  <div
    class="scrim"
    role="presentation"
    onclick={(e) => {
      if (e.target === e.currentTarget) confirmingFolder = null;
    }}
  >
    <div class="confirm" role="dialog" aria-modal="true" aria-labelledby="delete-folder-title">
      <p id="delete-folder-title" class="confirm-title">
        {#if count === 0}
          Delete "{confirmingFolder.name}"?
        {:else}
          Delete "{confirmingFolder.name}" and the {count}
          {count === 1 ? "note" : "notes"} in it?
        {/if}
      </p>
      <!-- The cascade is the whole reason this dialog exists: `delete_folder`
           deletes the folder and every note in it, both or neither. The count
           is named because "delete folder" reads like an empty operation. -->
      <p class="confirm-body">
        {#if count === 0}
          The folder is empty, so nothing else goes with it.
        {:else}
          The folder and all {count}
          {count === 1 ? "note" : "notes"} filed in it go for good — there's no trash to fish them
          back out of. Move anything you want to keep out of the folder first.
        {/if}
      </p>
      <div class="confirm-actions">
        <button class="btn-secondary" onclick={() => (confirmingFolder = null)}>Cancel</button>
        <button class="btn-danger" onclick={confirmDeleteFolder}>
          {count === 0 ? "Delete folder" : `Delete folder and ${count} ${count === 1 ? "note" : "notes"}`}
        </button>
      </div>
    </div>
  </div>
{/if}

<style>
  .notes-page {
    max-width: 940px;
  }

  /* ---------- Folder rail ---------- */
  .workspace {
    display: flex;
    align-items: flex-start;
    gap: 26px;
  }

  .rail {
    flex: none;
    width: 190px;
    display: flex;
    flex-direction: column;
    gap: 1px;
    background: var(--rail-bg);
    border: 1px solid var(--hairline);
    border-radius: var(--radius-card);
    padding: 8px;
  }

  .rail-item {
    display: flex;
    align-items: center;
    border-radius: var(--radius-control);
  }

  .rail-row {
    flex: 1;
    min-width: 0;
    display: flex;
    align-items: center;
    gap: 9px;
    text-align: left;
    border: none;
    background: transparent;
    border-radius: var(--radius-control);
    padding: 7px 9px;
    cursor: pointer;
    font-family: var(--font-ui);
    font-size: 13px;
    color: var(--fg);
  }

  .rail-row :global(svg) {
    flex: none;
    color: var(--fg-faint);
  }

  .rail-row:hover,
  .rail-item:hover {
    background: var(--wash);
  }

  .rail-row.selected,
  .rail-item.selected {
    background: var(--selected);
    font-weight: 600;
  }

  .rail-item.selected .rail-row {
    font-weight: 600;
  }

  .rail-name {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .rail-count {
    flex: none;
    font-size: 11.5px;
    font-weight: 500;
    color: var(--fg-faint);
    font-variant-numeric: tabular-nums;
  }

  .rail-actions {
    flex: none;
    display: flex;
    padding-right: 4px;
    opacity: 0;
    transition: opacity var(--motion);
  }

  .rail-item:hover .rail-actions,
  .rail-item:focus-within .rail-actions {
    opacity: 1;
  }

  .rail-actions .icon-btn {
    width: 24px;
    height: 24px;
  }

  .rail-sep {
    height: 1px;
    background: var(--hairline);
    margin: 7px 4px;
  }

  .rail-edit {
    display: flex;
    align-items: center;
    gap: 9px;
    padding: 6px 9px;
    color: var(--fg-faint);
  }

  .rail-edit :global(svg) {
    flex: none;
  }

  .rail-edit input {
    flex: 1;
    min-width: 0;
    border: none;
    outline: none;
    background: transparent;
    font-family: var(--font-ui);
    font-size: 13px;
    color: var(--fg);
    border-bottom: 1px solid var(--hairline-strong);
  }

  .rail-add {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-top: 3px;
    border: none;
    background: transparent;
    border-radius: var(--radius-control);
    padding: 7px 9px;
    cursor: pointer;
    font-family: var(--font-ui);
    font-size: 12.5px;
    font-weight: 550;
    color: var(--fg-muted);
    text-align: left;
  }

  .rail-add:hover {
    background: var(--wash);
    color: var(--fg);
  }

  .rail-error {
    margin: 8px 2px 2px;
    font-size: 12px;
    line-height: 1.45;
    color: var(--danger);
  }

  .list-col {
    flex: 1;
    min-width: 0;
  }

  .scope-note {
    margin: -10px 0 14px;
    font-size: 12px;
    color: var(--fg-faint);
  }

  .row-folder {
    flex: none;
    max-width: 130px;
    font-size: 11.5px;
    color: var(--fg-muted);
    background: var(--chip);
    border-radius: var(--radius-pill);
    padding: 2px 9px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  /* ---------- Editor folder picker ---------- */
  .editor-bar-right {
    display: flex;
    align-items: center;
    gap: 14px;
  }

  /* ---------- Notices ---------- */
  .notice {
    display: flex;
    align-items: flex-start;
    gap: 10px;
    background: var(--teal-soft);
    color: var(--teal);
    border-radius: var(--radius-card);
    padding: 12px 14px 12px 16px;
    font-size: 13px;
    line-height: 1.55;
    margin: -8px 0 20px;
  }

  .notice.caution {
    background: var(--caution-soft);
    border: 1px solid var(--caution-line);
    color: var(--caution);
  }

  .notice :global(svg) {
    flex: none;
    margin-top: 1px;
  }

  .notice span {
    flex: 1;
  }

  .notice-close {
    flex: none;
    border: none;
    background: transparent;
    color: inherit;
    padding: 2px;
    cursor: pointer;
    border-radius: 50%;
    opacity: 0.7;
  }

  .notice-close:hover {
    opacity: 1;
  }

  .save-error {
    background: var(--danger-soft);
    color: var(--danger);
    border-radius: var(--radius-control);
    padding: 10px 14px;
    font-size: 13px;
    line-height: 1.5;
    margin: 0 0 16px;
  }

  /* ---------- Toolbar ---------- */
  .toolbar {
    display: flex;
    align-items: center;
    gap: 14px;
    margin-bottom: 20px;
  }

  .search-box {
    flex: 1;
    min-width: 0;
    display: flex;
    align-items: center;
    gap: 8px;
    background: var(--surface);
    border: 1px solid var(--hairline-strong);
    border-radius: var(--radius-control);
    padding: 9px 12px;
    color: var(--fg-faint);
  }

  .search-box input {
    flex: 1;
    min-width: 0;
    border: none;
    outline: none;
    background: transparent;
    font-family: var(--font-ui);
    font-size: 13.5px;
    color: var(--fg);
  }

  .search-box input::placeholder {
    color: var(--fg-faint);
  }

  .clear-search {
    flex: none;
    border: none;
    background: transparent;
    color: var(--fg-faint);
    padding: 2px;
    cursor: pointer;
    border-radius: 50%;
  }

  .clear-search:hover {
    color: var(--fg);
    background: var(--wash);
  }

  .new-note {
    flex: none;
    display: flex;
    align-items: center;
    gap: 7px;
    font-family: var(--font-ui);
    font-size: 13px;
    font-weight: 600;
    border: none;
    border-radius: var(--radius-control);
    background: var(--accent);
    color: var(--accent-fg);
    padding: 9px 15px;
    cursor: pointer;
  }

  .new-note:hover {
    opacity: 0.9;
  }

  /* ---------- List ---------- */
  .loading-line {
    color: var(--fg-faint);
    font-size: 13px;
    padding: 20px 0;
  }

  .note-list {
    list-style: none;
    margin: 0;
    padding: 0;
    border: 1px solid var(--hairline);
    border-radius: var(--radius-card);
    background: var(--surface);
    overflow: hidden;
  }

  .note-list li {
    display: flex;
    align-items: center;
    gap: 6px;
    border-top: 1px solid var(--hairline);
  }

  .note-list li:first-child {
    border-top: none;
  }

  .row {
    flex: 1;
    min-width: 0;
    display: flex;
    align-items: baseline;
    gap: 16px;
    text-align: left;
    border: none;
    background: transparent;
    padding: 14px 8px 14px 18px;
    cursor: pointer;
    font-family: var(--font-ui);
    color: var(--fg);
  }

  .row:hover {
    background: var(--wash-soft);
  }

  .row-main {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 3px;
  }

  .row-title {
    font-size: 14px;
    font-weight: 600;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .row-title.untitled {
    color: var(--fg-faint);
    font-weight: 450;
  }

  .row-preview {
    font-size: 12.5px;
    color: var(--fg-faint);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .row-time {
    flex: none;
    font-size: 12.5px;
    color: var(--fg-faint);
    white-space: nowrap;
  }

  .row-actions {
    flex: none;
    display: flex;
    padding-right: 12px;
    opacity: 0;
    transition: opacity var(--motion);
  }

  .note-list li:hover .row-actions,
  .note-list li:focus-within .row-actions {
    opacity: 1;
  }

  .icon-btn {
    appearance: none;
    width: 28px;
    height: 28px;
    display: grid;
    place-items: center;
    border: none;
    background: transparent;
    color: var(--fg-faint);
    padding: 0;
    cursor: pointer;
    border-radius: 6px;
    transition: background var(--motion), color var(--motion);
  }

  .icon-btn:hover {
    color: var(--fg);
    background: var(--wash);
  }

  .icon-btn.danger:hover {
    color: var(--danger);
    background: var(--danger-soft);
  }

  .load-more {
    display: block;
    margin: 18px auto 0;
    font-family: var(--font-ui);
    font-size: 13px;
    font-weight: 600;
    color: var(--fg);
    background: var(--surface);
    border: 1px solid var(--hairline-strong);
    border-radius: var(--radius-control);
    padding: 8px 18px;
    cursor: pointer;
  }

  .load-more:hover {
    background: var(--bg-elevated);
  }

  .load-more:disabled {
    opacity: 0.5;
    cursor: default;
  }

  /* ---------- Editor ---------- */
  .editor {
    display: flex;
    flex-direction: column;
    min-height: 0;
  }

  .editor-bar {
    display: flex;
    align-items: center;
    justify-content: space-between;
    margin-bottom: 18px;
  }

  .ghost {
    background: transparent;
    border: 1px solid var(--hairline);
    color: var(--fg);
    border-radius: var(--radius-control);
    padding: 6px 12px;
    font-family: var(--font-ui);
    font-size: 13px;
    font-weight: 500;
    cursor: pointer;
    transition: background var(--motion), border-color var(--motion);
  }

  .ghost:hover {
    background: var(--wash);
    border-color: var(--hairline-strong);
  }

  .save-state {
    color: var(--fg-faint);
    font-size: 12px;
    font-variant-numeric: tabular-nums;
    transition: color var(--motion);
  }

  .save-state.busy {
    color: var(--fg-muted);
  }

  /* ---------- Actions and auto-title ---------- */
  .ghost.tool {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    padding: 5px 10px;
    font-size: 12.5px;
    color: var(--fg-muted);
  }

  .ghost.tool:disabled {
    opacity: 0.5;
    cursor: default;
  }

  .menu-wrap {
    position: relative;
    display: inline-flex;
  }

  .action-menu {
    position: absolute;
    top: calc(100% + 6px);
    right: 0;
    z-index: 40;
    min-width: 240px;
    max-width: 320px;
    display: flex;
    flex-direction: column;
    padding: 4px;
    background: var(--surface);
    border: 1px solid var(--hairline-strong);
    border-radius: var(--radius-card);
    box-shadow: 0 10px 30px rgb(0 0 0 / 12%);
  }

  .menu-item {
    display: flex;
    align-items: flex-start;
    gap: 10px;
    width: 100%;
    padding: 8px 10px;
    background: transparent;
    border: none;
    border-radius: 8px;
    color: var(--fg);
    font-family: var(--font-ui);
    text-align: left;
    cursor: pointer;
  }

  .menu-item:hover {
    background: var(--wash);
  }

  .menu-item.manage {
    border-top: 1px solid var(--hairline);
    border-radius: 0 0 8px 8px;
    color: var(--fg-muted);
  }

  .menu-text {
    display: flex;
    flex-direction: column;
    gap: 2px;
    min-width: 0;
  }

  .menu-name {
    font-size: 13px;
    font-weight: 600;
  }

  .menu-desc {
    font-size: 12px;
    line-height: 1.4;
    color: var(--fg-muted);
  }

  .enhanced {
    margin-top: 18px;
    padding-top: 14px;
    border-top: 1px solid var(--hairline);
  }

  .enhanced-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
  }

  .enhanced-label {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    font-size: 11.5px;
    font-weight: 650;
    letter-spacing: 0.03em;
    text-transform: uppercase;
    color: var(--fg-faint);
  }

  .enhanced-input {
    min-height: 240px;
    padding-top: 10px;
  }

  .manager-panel {
    margin-top: 20px;
    padding-top: 18px;
    border-top: 1px solid var(--hairline);
  }

  .title-input {
    width: 100%;
    border: none;
    outline: none;
    background: transparent;
    font-family: var(--font-ui);
    font-size: 24px;
    font-weight: 650;
    letter-spacing: -0.02em;
    color: var(--fg);
    padding: 0 0 12px;
    margin-bottom: 4px;
    border-bottom: 1px solid var(--hairline);
  }

  .title-input::placeholder {
    color: var(--fg-faint);
    font-weight: 450;
  }

  .content-input {
    width: 100%;
    min-height: 360px;
    background: transparent;
    border: none;
    outline: none;
    padding: 16px 0 0;
    font-family: var(--font-ui);
    font-size: 14.5px;
    line-height: 1.7;
    color: var(--fg);
    resize: vertical;
    user-select: text;
    cursor: text;
  }

  .content-input::placeholder {
    color: var(--fg-faint);
  }

  /* ---------- Delete confirmation ---------- */
  .scrim {
    position: fixed;
    inset: 0;
    z-index: 60;
    background: var(--scrim);
    display: grid;
    place-items: center;
    padding: 24px;
  }

  .confirm {
    width: min(400px, 100%);
    background: var(--surface);
    border: 1px solid var(--hairline);
    border-radius: var(--radius-card);
    box-shadow: var(--shadow-modal);
    padding: 22px 24px 20px;
  }

  .confirm-title {
    font-size: 15px;
    font-weight: 650;
    color: var(--fg);
    margin: 0 0 8px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .confirm-body {
    font-size: 13px;
    line-height: 1.55;
    color: var(--fg-muted);
    margin: 0 0 20px;
  }

  .confirm-actions {
    display: flex;
    justify-content: flex-end;
    gap: 10px;
  }

  .btn-secondary,
  .btn-danger {
    font-family: var(--font-ui);
    font-size: 13px;
    font-weight: 600;
    border-radius: var(--radius-control);
    padding: 8px 16px;
    cursor: pointer;
  }

  .btn-secondary {
    background: var(--surface);
    border: 1px solid var(--hairline-strong);
    color: var(--fg);
  }

  .btn-secondary:hover {
    background: var(--bg-elevated);
  }

  .btn-danger {
    background: var(--danger);
    border: 1px solid transparent;
    color: var(--danger-fg);
  }

  .btn-danger:hover {
    opacity: 0.88;
  }
</style>
