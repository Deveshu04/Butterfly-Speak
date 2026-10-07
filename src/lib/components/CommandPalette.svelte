<script module lang="ts">
  import type { Folder, HistoryEntry, Note } from "$lib/api";

  /** One result the palette can open. The shell routes it to its page. */
  export type PaletteItem =
    | { kind: "folder"; folder: Folder }
    | { kind: "note"; note: Note }
    | { kind: "dictation"; entry: HistoryEntry };
</script>

<script lang="ts">
  import { onMount, untrack } from "svelte";
  import { historySearch, listFolders, listNotes, searchNotes } from "$lib/api";
  import Icon from "$lib/components/Icon.svelte";

  let { onclose, onselect }: { onclose: () => void; onselect: (item: PaletteItem) => void } =
    $props();

  // Per-kind caps, sized to the panel: 66vh tall, one-line rows of about
  // 33 px, two-line note rows of about 49 px, headings of about 27 px. Full
  // dictation and folder sections stay above the fold even in the smallest
  // window; notes come last and get one screenful.
  const DICTATION_CAP = 4;
  const FOLDER_CAP = 3;
  const NOTE_CAP = 8;
  /** Quiet time after the last keystroke before the three searches go out:
   * longer than the gap between keys inside a word at ordinary typing speed,
   * short enough that the list follows the last key without a visible wait. */
  const SETTLE_MS = 350;

  type Kind = "recent" | "folders" | "notes" | "dictations";

  /** The query the rows on screen answer. Empty means recent notes. */
  let shownQuery = $state("");
  let recent = $state<Note[]>([]);
  let recentLoaded = $state(false);
  /** Every folder, for matching names and for labelling note rows. */
  let folders = $state<Folder[]>([]);
  let dictationHits = $state<HistoryEntry[]>([]);
  let folderHits = $state<Folder[]>([]);
  let noteHits = $state<Note[]>([]);
  /** Kinds that have not answered `shownQuery` yet. The no-match line waits
   * for all of them. */
  let unanswered = $state(0);
  let highlighted = $state(0);

  let inputEl = $state<HTMLInputElement | null>(null);
  let resultsEl = $state<HTMLDivElement | null>(null);
  let settleTimer: ReturnType<typeof setTimeout> | undefined;

  /** The newest request per kind. A response is applied only when it answers
   * that request, and the kinds never cancel one another. */
  const newestRequest: Record<Kind, number> = { recent: 0, folders: 0, notes: 0, dictations: 0 };
  /** Whether the person has moved the highlight since the current query went
   * out. Until they do, a change in the rows puts it back on the first row;
   * after, it stays with the row they picked. */
  let steered = false;
  /** Row keys the highlight was last placed against. */
  let placedKeys: string[] = [];
  let lastPointer = { x: Number.NaN, y: Number.NaN };
  let chosen = false;

  const folderList = listFolders();

  /** The flat list, in the order it is drawn. The highlight is a position in
   * it. */
  let rows: PaletteItem[] = $derived(
    shownQuery === ""
      ? recent.map((note): PaletteItem => ({ kind: "note", note }))
      : [
          ...dictationHits.map((entry): PaletteItem => ({ kind: "dictation", entry })),
          ...folderHits.map((folder): PaletteItem => ({ kind: "folder", folder })),
          ...noteHits.map((note): PaletteItem => ({ kind: "note", note })),
        ],
  );

  function keyOf(item: PaletteItem): string {
    if (item.kind === "folder") return `f${item.folder.id}`;
    if (item.kind === "note") return `n${item.note.id}`;
    return `d${item.entry.id}`;
  }

  $effect.pre(() => {
    const keys = rows.map(keyOf);
    untrack(() => placeHighlight(keys));
  });

  $effect(() => {
    const index = highlighted;
    void rows.length;
    resultsEl
      ?.querySelector<HTMLElement>(`[data-row="${index}"]`)
      ?.scrollIntoView({ block: "nearest" });
  });

  /** Keep the highlight where it belongs after the rows change: nowhere new
   * when the same rows came back, the first row when they changed and the
   * person has not steered, and the same row when they have. A row that is
   * gone leaves the highlight on the last row at most. */
  function placeHighlight(keys: string[]) {
    const before = placedKeys;
    placedKeys = keys;
    if (keys.length === before.length && keys.every((k, i) => k === before[i])) return;
    if (!steered) {
      highlighted = 0;
      return;
    }
    const kept = keys.indexOf(before[highlighted] ?? "");
    highlighted = kept >= 0 ? kept : Math.max(0, Math.min(highlighted, keys.length - 1));
  }

  /** Run one kind's lookup. A failure reads as no results, and the log line
   * never carries the query or anything found. */
  async function lookUp<T>(kind: Kind, load: () => Promise<T[]>, apply: (found: T[]) => void) {
    const request = ++newestRequest[kind];
    let found: T[] = [];
    try {
      found = await load();
    } catch {
      if (request === newestRequest[kind]) console.error(`command palette: ${kind} lookup failed`);
    }
    if (request === newestRequest[kind]) apply(found);
  }

  function matchFolders(all: Folder[], needle: string): Folder[] {
    const lower = needle.toLocaleLowerCase();
    return all.filter((f) => f.name.toLocaleLowerCase().includes(lower)).slice(0, FOLDER_CAP);
  }

  function search(q: string) {
    shownQuery = q;
    steered = false;
    unanswered = 3;
    const answered = () => (unanswered = Math.max(0, unanswered - 1));
    void lookUp("dictations", () => historySearch(q, DICTATION_CAP), (found) => {
      dictationHits = found;
      answered();
    });
    void lookUp("folders", async () => matchFolders(await folderList, q), (found) => {
      folderHits = found;
      answered();
    });
    void lookUp("notes", () => searchNotes(q, NOTE_CAP), (found) => {
      noteHits = found;
      answered();
    });
  }

  function showRecent() {
    shownQuery = "";
    steered = false;
    unanswered = 0;
    // Answers still on their way for the old query are dropped when they land.
    newestRequest.dictations += 1;
    newestRequest.folders += 1;
    newestRequest.notes += 1;
    dictationHits = [];
    folderHits = [];
    noteHits = [];
  }

  function onQueryInput(value: string) {
    clearTimeout(settleTimer);
    const q = value.trim();
    if (q === "") {
      showRecent();
    } else if (q !== shownQuery) {
      settleTimer = setTimeout(() => search(q), SETTLE_MS);
    }
  }

  // ---- Choosing -------------------------------------------------------------

  function choose(item: PaletteItem) {
    if (chosen) return;
    chosen = true;
    onselect(item);
  }

  /** Up and Down stop at the ends of the list rather than wrapping. */
  function moveHighlight(step: number) {
    if (rows.length === 0) return;
    highlighted = Math.max(0, Math.min(rows.length - 1, highlighted + step));
    steered = true;
  }

  function steerTo(index: number) {
    highlighted = index;
    steered = true;
  }

  /** Only a real movement of the mouse moves the highlight: rows sliding
   * under a cursor that stands still must not. */
  function onRowPointer(index: number, e: MouseEvent) {
    if (e.clientX === lastPointer.x && e.clientY === lastPointer.y) return;
    lastPointer = { x: e.clientX, y: e.clientY };
    steerTo(index);
  }

  /** Captured at the window, so the keys work wherever focus is inside the
   * palette and nothing behind it reacts to them as well. */
  function onKeydown(e: KeyboardEvent) {
    if (e.isComposing) return;
    if (e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      onclose();
    } else if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      e.stopPropagation();
      moveHighlight(e.key === "ArrowDown" ? 1 : -1);
    } else if (e.key === "Enter") {
      // A row button with keyboard focus opens itself: Enter on a button is
      // its click, and that click is the only activation.
      if (e.target instanceof Element && e.target.closest(".row")) return;
      e.preventDefault();
      e.stopPropagation();
      const item = rows[highlighted];
      if (item) choose(item);
    }
  }

  // ---- Presentation ---------------------------------------------------------

  function firstFilledLine(text: string): string {
    for (const line of text.split("\n")) {
      const trimmed = line.trim();
      if (trimmed) return trimmed;
    }
    return "";
  }

  /** The line under a note's title: the body's first non-blank line, or the
   * next one when the first only repeats the title. */
  function noteExcerpt(note: Note): string {
    const title = note.title.trim();
    let first = true;
    for (const line of note.content.split("\n")) {
      const trimmed = line.trim();
      if (!trimmed) continue;
      if (first && title && trimmed.replace(/^#+\s*/, "") === title) {
        first = false;
        continue;
      }
      return trimmed;
    }
    return "";
  }

  function folderNameOf(id: number | null): string | null {
    if (id === null) return null;
    return folders.find((f) => f.id === id)?.name ?? null;
  }

  function noteCount(n: number): string {
    return `${n} ${n === 1 ? "note" : "notes"}`;
  }

  /** `createdAt` is SQLite's `YYYY-MM-DD HH:MM:SS`, in UTC. Today's
   * dictations show the time; older ones the date, with the year only when it
   * is not this one. */
  function dictationTime(createdAt: string): string {
    const at = new Date(`${createdAt.replace(" ", "T")}Z`);
    if (Number.isNaN(at.getTime())) return "";
    const now = new Date();
    if (at.toDateString() === now.toDateString()) {
      return at.toLocaleTimeString(undefined, { hour: "numeric", minute: "2-digit" });
    }
    return at.getFullYear() === now.getFullYear()
      ? at.toLocaleDateString(undefined, { month: "short", day: "numeric" })
      : at.toLocaleDateString(undefined, { month: "short", day: "numeric", year: "numeric" });
  }

  onMount(() => {
    inputEl?.focus();
    void lookUp("recent", () => listNotes({ page: 0 }), (found) => {
      recent = found;
      recentLoaded = true;
    });
    folderList.then(
      (all) => (folders = all),
      () => console.error("command palette: folder list failed to load"),
    );
    return () => clearTimeout(settleTimer);
  });
</script>

<svelte:window onkeydowncapture={onKeydown} />

{#snippet resultRow(item: PaletteItem, index: number)}
  <button
    type="button"
    class="row"
    class:active={index === highlighted}
    data-row={index}
    onclick={() => choose(item)}
    onmousemove={(e) => onRowPointer(index, e)}
    onfocus={() => steerTo(index)}
  >
    {#if item.kind === "dictation"}
      {@const line = firstFilledLine(item.entry.text)}
      <Icon name="mic" size={16} stroke={1.7} />
      <span class="row-main">
        <span class="row-title" class:untitled={!line}>{line || "Empty dictation"}</span>
      </span>
      <span class="row-meta">{dictationTime(item.entry.createdAt)}</span>
    {:else if item.kind === "folder"}
      <Icon name="folder" size={16} stroke={1.7} />
      <span class="row-main">
        <span class="row-title">{item.folder.name}</span>
      </span>
      <span class="row-meta">{noteCount(item.folder.noteCount)}</span>
    {:else}
      {@const title = item.note.title.trim()}
      {@const excerpt = noteExcerpt(item.note)}
      {@const folderName = folderNameOf(item.note.folderId)}
      <Icon name="note" size={16} stroke={1.7} />
      <span class="row-main">
        <span class="row-title" class:untitled={!title}>{title || "Untitled"}</span>
        {#if excerpt}
          <span class="row-sub">{excerpt}</span>
        {/if}
      </span>
      {#if folderName}
        <span class="row-meta">{folderName}</span>
      {/if}
    {/if}
  </button>
{/snippet}

<div
  class="scrim"
  role="presentation"
  onclick={(e) => {
    if (e.target === e.currentTarget) onclose();
  }}
>
  <div class="palette" role="dialog" aria-modal="true" aria-label="Go to a note, folder or dictation">
    <div class="search-row">
      <Icon name="search" size={17} stroke={1.8} />
      <input
        bind:this={inputEl}
        type="text"
        placeholder="Find a note, folder or dictation…"
        aria-label="Search notes, folders and dictations"
        spellcheck="false"
        autocomplete="off"
        oninput={(e) => onQueryInput(e.currentTarget.value)}
      />
    </div>

    <div class="results" bind:this={resultsEl}>
      {#if shownQuery === ""}
        {#if recent.length > 0}
          <p class="section">Recent notes</p>
          {#each recent as note, i (note.id)}
            {@render resultRow({ kind: "note", note }, i)}
          {/each}
        {:else if recentLoaded}
          <p class="empty">No notes yet. Type to search your folders and past dictations.</p>
        {/if}
      {:else}
        {#if dictationHits.length > 0}
          <p class="section">Dictations</p>
          {#each dictationHits as entry, i (entry.id)}
            {@render resultRow({ kind: "dictation", entry }, i)}
          {/each}
        {/if}
        {#if folderHits.length > 0}
          <p class="section">Folders</p>
          {#each folderHits as folder, i (folder.id)}
            {@render resultRow({ kind: "folder", folder }, dictationHits.length + i)}
          {/each}
        {/if}
        {#if noteHits.length > 0}
          <p class="section">Notes</p>
          {#each noteHits as note, i (note.id)}
            {@render resultRow({ kind: "note", note }, dictationHits.length + folderHits.length + i)}
          {/each}
        {/if}
        {#if rows.length === 0 && unanswered === 0}
          <p class="empty">Nothing in your folders, notes or dictations matches “{shownQuery}”.</p>
        {/if}
      {/if}
    </div>

    <div class="foot">
      <span><kbd>↑</kbd><kbd>↓</kbd> move</span>
      <span><kbd>Enter</kbd> open</span>
      <span><kbd>Esc</kbd> close</span>
    </div>
  </div>
</div>

<style>
  .scrim {
    position: fixed;
    inset: 0;
    z-index: 90;
    background: var(--scrim);
    display: flex;
    justify-content: center;
    align-items: flex-start;
    padding: 12vh 24px 24px;
  }

  .palette {
    width: min(620px, 100%);
    max-height: 66vh;
    display: flex;
    flex-direction: column;
    background: var(--surface);
    border: 1px solid var(--hairline);
    border-radius: var(--radius-card);
    box-shadow: var(--shadow-modal);
    overflow: hidden;
  }

  .search-row {
    flex: none;
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 14px 16px;
    border-bottom: 1px solid var(--hairline);
    color: var(--fg-faint);
  }

  .search-row input {
    flex: 1;
    min-width: 0;
    border: none;
    outline: none;
    background: transparent;
    font-family: var(--font-ui);
    font-size: 15px;
    color: var(--fg);
  }

  .search-row input::placeholder {
    color: var(--fg-faint);
  }

  .results {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    padding: 6px 0 8px;
  }

  .empty {
    color: var(--fg-faint);
    font-size: 13px;
    padding: 22px 18px;
    margin: 0;
    text-align: center;
  }

  .section {
    margin: 10px 0 4px;
    padding: 0 18px;
    font-size: 11px;
    font-weight: 650;
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--fg-faint);
  }

  .row {
    width: 100%;
    display: flex;
    align-items: center;
    gap: 11px;
    text-align: left;
    border: none;
    background: transparent;
    padding: 8px 18px;
    cursor: pointer;
    font-family: var(--font-ui);
    color: var(--fg);
  }

  .row :global(svg) {
    flex: none;
    color: var(--fg-faint);
  }

  .row.active {
    background: var(--selected);
  }

  .row.active :global(svg) {
    color: var(--fg);
  }

  .row-main {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }

  .row-title {
    font-size: 13.5px;
    font-weight: 550;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .row-title.untitled {
    color: var(--fg-faint);
    font-weight: 450;
  }

  .row-sub {
    font-size: 12px;
    color: var(--fg-faint);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .row-meta {
    flex: none;
    max-width: 34%;
    font-size: 11.5px;
    color: var(--fg-faint);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .foot {
    flex: none;
    display: flex;
    gap: 16px;
    align-items: center;
    padding: 9px 18px;
    border-top: 1px solid var(--hairline);
    background: var(--bg-elevated);
    font-size: 11.5px;
    color: var(--fg-faint);
  }

  kbd {
    font-family: var(--font-ui);
    font-size: 10.5px;
    font-weight: 600;
    background: var(--chip);
    border: 1px solid var(--hairline-strong);
    border-radius: 5px;
    padding: 1px 5px;
    margin-right: 3px;
    color: var(--fg-muted);
  }

  .foot span {
    display: inline-flex;
    align-items: center;
  }
</style>
