<script lang="ts">
  import { onDestroy, onMount } from "svelte";
  import {
    historyClear,
    historyDelete,
    historyList,
    historySearch,
    type HistoryEntry,
  } from "$lib/api";
  import { openRequest } from "$lib/openRequest.svelte";
  import { settings } from "$lib/stores.svelte";
  import { stats } from "$lib/stats.svelte";
  import EmptyState from "$lib/components/EmptyState.svelte";
  import Icon from "$lib/components/Icon.svelte";

  const PAGE_SIZE = 30;
  const SEARCH_LIMIT = 200;
  const SEARCH_DEBOUNCE_MS = 250;

  /** A failed row's `errorCode`, the `ERR_*` constants in controller.rs, as
   * a sentence. Nothing from a failed row was typed anywhere. */
  const FAILURES: Record<string, string> = {
    "connection-lost":
      "The connection dropped partway through, so this is only part of what you said, and none of it was typed.",
    "route-unavailable": "The command couldn't be carried out, so nothing was typed.",
    "route-timeout": "This took too long to finish, so nothing was typed.",
    "interrupted": "The computer went to sleep before this could finish, so nothing was typed.",
  };

  let entries = $state<HistoryEntry[]>([]);
  let query = $state("");
  let page = $state(0);
  let hasMore = $state(false);
  let loading = $state(false);
  let loadedOnce = $state(false);
  let copiedId = $state<number | null>(null);
  let drawerEntry = $state<HistoryEntry | null>(null);
  let confirmingClear = $state(false);
  let clearing = $state(false);
  let clearFailed = $state(false);

  let searchTimer: ReturnType<typeof setTimeout> | undefined;
  let requestToken = 0;

  let searching = $derived(query.trim().length > 0);
  let retentionOff = $derived(settings.current ? !settings.current.history.enabled : false);

  /** SQLite's `datetime('now')` is UTC with no offset marker — treat it as
   * such explicitly, since `new Date("YYYY-MM-DD HH:MM:SS")` (no "Z") is
   * parsed as *local* time by every engine this app ships on. */
  function parseUtc(s: string): Date {
    return new Date(s.replace(" ", "T") + "Z");
  }

  function dateGroupLabel(d: Date): string {
    const now = new Date();
    const startOfDay = (x: Date) => new Date(x.getFullYear(), x.getMonth(), x.getDate()).getTime();
    const diffDays = Math.round((startOfDay(now) - startOfDay(d)) / 86_400_000);
    if (diffDays === 0) return "Today";
    if (diffDays === 1) return "Yesterday";
    const opts: Intl.DateTimeFormatOptions = { month: "long", day: "numeric" };
    if (d.getFullYear() !== now.getFullYear()) opts.year = "numeric";
    return d.toLocaleDateString(undefined, opts);
  }

  function fmtTime(s: string): string {
    return parseUtc(s)
      .toLocaleTimeString([], { hour: "numeric", minute: "2-digit" })
      .toLowerCase();
  }

  function fmtDuration(ms: number | null): string | null {
    if (ms == null) return null;
    const secs = ms / 1000;
    if (secs < 60) return `${secs.toFixed(1)}s`;
    const m = Math.floor(secs / 60);
    const rem = Math.round(secs % 60);
    return `${m}m ${rem.toString().padStart(2, "0")}s`;
  }

  let groups = $derived.by(() => {
    const order: string[] = [];
    const buckets = new Map<string, HistoryEntry[]>();
    for (const e of entries) {
      const label = dateGroupLabel(parseUtc(e.createdAt));
      if (!buckets.has(label)) {
        buckets.set(label, []);
        order.push(label);
      }
      buckets.get(label)!.push(e);
    }
    return order.map((label) => ({ label, items: buckets.get(label)! }));
  });

  async function runSearch() {
    const token = ++requestToken;
    loading = true;
    try {
      const rows = await historySearch(query.trim(), SEARCH_LIMIT);
      if (token !== requestToken) return; // a newer request already landed
      entries = rows;
      hasMore = false;
    } catch (e) {
      console.error("history search failed:", e);
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
      const rows = await historyList(targetPage, PAGE_SIZE);
      if (token !== requestToken) return;
      entries = reset ? rows : [...entries, ...rows];
      page = targetPage;
      hasMore = rows.length === PAGE_SIZE;
    } catch (e) {
      console.error("history list failed:", e);
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

  async function copy(text: string, id: number) {
    try {
      await navigator.clipboard.writeText(text);
      copiedId = id;
      setTimeout(() => (copiedId = null), 1200);
    } catch {
      /* clipboard unavailable — ignore */
    }
  }

  async function remove(id: number) {
    const ok = await historyDelete(id);
    if (ok) {
      entries = entries.filter((e) => e.id !== id);
      if (drawerEntry?.id === id) drawerEntry = null;
    }
  }

  /** "Clear all history": every row in the history file goes, not only the
   * ones on this page or matching the search. The backend also has the
   * controller forget the last dictation it keeps for Paste last transcript,
   * and the dictations Home holds in memory while history is off go too. */
  async function clearAll() {
    if (clearing) return;
    clearing = true;
    clearFailed = false;
    try {
      await historyClear();
      stats.clearSession();
      confirmingClear = false;
      drawerEntry = null;
      query = "";
      reload();
    } catch (e) {
      console.error("history clear failed:", e);
      clearFailed = true;
    } finally {
      clearing = false;
    }
  }

  function openDrawer(entry: HistoryEntry) {
    drawerEntry = entry;
  }

  function closeDrawer() {
    drawerEntry = null;
  }

  let drawerHasEdit = $derived(
    !!drawerEntry?.rawText && drawerEntry.rawText.trim() !== drawerEntry.text.trim(),
  );

  /**
   * A dictation picked in the Ctrl+K palette lands here.
   *
   * An `$effect` rather than an `onMount` read, because both arrivals have to
   * work: the palette can be opened from the History page itself (this
   * component is already mounted and `onMount` will never run again), or from
   * anywhere else (the shell navigates, and this component is created by that
   * navigation — the effect runs on mount too). The row travels whole, so
   * there is nothing to re-fetch and the drawer can open on an entry that is
   * not in the current page of the list.
   */
  $effect(() => {
    const request = openRequest.take("dictation");
    if (request) drawerEntry = request.entry;
  });

  onMount(() => {
    reload();
  });

  onDestroy(() => {
    if (searchTimer) clearTimeout(searchTimer);
  });
</script>

<div class="page history-page">
  <h1 class="page-title">History</h1>
  <p class="page-desc">Everything you've dictated, searchable and kept on this device.</p>

  {#if retentionOff}
    <div class="retention-banner">
      <Icon name="help" size={16} stroke={1.8} />
      <span>
        History isn't being saved — turn on <strong>Keep dictation history</strong> in
        Settings → System to start recording new dictations here.
      </span>
    </div>
  {/if}

  <div class="toolbar">
    <div class="search-box">
      <Icon name="search" size={16} stroke={1.7} />
      <input
        type="text"
        placeholder="Search your dictations…"
        aria-label="Search history"
        autocomplete="off"
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
    {#if entries.length > 0 || searching}
      <button
        class="clear-all"
        onclick={() => {
          clearFailed = false;
          confirmingClear = true;
        }}
      >
        Clear all history
      </button>
    {/if}
  </div>

  {#if !loadedOnce && loading}
    <p class="loading-line">Loading…</p>
  {:else if entries.length === 0}
    {#if searching}
      <EmptyState
        icon="search"
        title="No matches"
        body={`Nothing in your history matches "${query.trim()}".`}
      />
    {:else}
      <EmptyState
        icon="clock"
        title="No dictations yet"
        body="Everything you dictate will show up here, grouped by day and searchable."
      />
    {/if}
  {:else}
    <div class="groups">
      {#each groups as group (group.label)}
        <section class="group">
          <p class="group-label">{group.label}</p>
          <ul>
            {#each group.items as entry (entry.id)}
              <li>
                <button class="row" onclick={() => openDrawer(entry)}>
                  <span class="time">{fmtTime(entry.createdAt)}</span>
                  <span class="text">{entry.text || "(empty)"}</span>
                  <span class="meta">
                    {#if entry.app}
                      <span class="chip">{entry.app}</span>
                    {/if}
                    {#if entry.provider}
                      <span class="chip provider">{entry.provider}</span>
                    {/if}
                    {#if fmtDuration(entry.durationMs)}
                      <span class="chip">{fmtDuration(entry.durationMs)}</span>
                    {/if}
                    {#if entry.outcome === "failed"}
                      <span class="chip failed">Failed</span>
                    {/if}
                  </span>
                </button>
                <div class="row-actions">
                  <button
                    class="icon-btn"
                    class:done={copiedId === entry.id}
                    aria-label="Copy"
                    title="Copy"
                    onclick={() => copy(entry.text, entry.id)}
                  >
                    <Icon name="copy" size={15} stroke={1.6} />
                  </button>
                  <button
                    class="icon-btn danger"
                    aria-label="Delete"
                    title="Delete"
                    onclick={() => remove(entry.id)}
                  >
                    <Icon name="trash" size={15} stroke={1.6} />
                  </button>
                </div>
              </li>
            {/each}
          </ul>
        </section>
      {/each}
    </div>

    {#if hasMore}
      <button class="load-more" disabled={loading} onclick={loadMore}>
        {loading ? "Loading…" : "Load more"}
      </button>
    {/if}
  {/if}
</div>

{#if drawerEntry}
  <div
    class="scrim"
    role="presentation"
    onclick={(e) => {
      if (e.target === e.currentTarget) closeDrawer();
    }}
  >
    <div class="drawer" role="dialog" aria-modal="true" aria-label="Dictation detail">
      <div class="drawer-head">
        <div>
          <p class="drawer-date">
            {dateGroupLabel(parseUtc(drawerEntry.createdAt))} · {fmtTime(drawerEntry.createdAt)}
          </p>
          <div class="drawer-chips">
            {#if drawerEntry.app}<span class="chip">{drawerEntry.app}</span>{/if}
            {#if drawerEntry.provider}<span class="chip provider">{drawerEntry.provider}</span>{/if}
            {#if fmtDuration(drawerEntry.durationMs)}
              <span class="chip">{fmtDuration(drawerEntry.durationMs)}</span>
            {/if}
          </div>
        </div>
        <button class="icon-btn" aria-label="Close" onclick={closeDrawer}>
          <Icon name="close" size={16} stroke={1.8} />
        </button>
      </div>

      {#if drawerEntry.outcome === "failed"}
        <p class="drawer-error">
          {FAILURES[drawerEntry.errorCode ?? ""] ?? "This dictation failed, so nothing was typed."}
        </p>
      {:else if drawerEntry.errorCode === "cloud-limit"}
        <!-- Pasted, so not a failure — but Cloud's limit ended it, and the
             words stop there rather than where the speaker stopped. -->
        <p class="drawer-error">
          Cloud's limit ended this dictation early — this is everything said before it stopped.
        </p>
      {/if}

      {#if drawerHasEdit}
        <div class="diff">
          <div class="diff-pane">
            <p class="diff-label">Raw transcript</p>
            <p class="diff-text">{drawerEntry.rawText}</p>
          </div>
          <!-- Not "after AI Polish": corrections, snippets, the style and an
               edit on Home change the text too. -->
          <div class="diff-pane">
            <p class="diff-label">Final text</p>
            <p class="diff-text">{drawerEntry.text}</p>
          </div>
        </div>
      {:else}
        <p class="no-edit">No AI processing — this is exactly what you said.</p>
        <p class="diff-text standalone">{drawerEntry.text}</p>
      {/if}

      <div class="drawer-actions">
        <button class="btn-secondary" onclick={() => copy(drawerEntry!.text, drawerEntry!.id)}>
          {copiedId === drawerEntry.id ? "Copied" : "Copy"}
        </button>
        <button
          class="btn-danger"
          onclick={() => {
            const id = drawerEntry!.id;
            remove(id);
          }}
        >
          Delete
        </button>
      </div>
    </div>
  </div>
{/if}

{#if confirmingClear}
  <div
    class="scrim confirm-scrim"
    role="presentation"
    onclick={(e) => {
      if (e.target === e.currentTarget && !clearing) confirmingClear = false;
    }}
  >
    <div class="confirm" role="dialog" aria-modal="true" aria-labelledby="clear-history-title">
      <p id="clear-history-title" class="confirm-title">Clear all history?</p>
      <p class="confirm-body">
        Every dictation saved in History is deleted from History on this device, including the
        ones not shown on this page. Home's list and Paste last transcript forget them too. Your
        notes, your dictionary, the words Speak is still learning from, the word counts behind
        Insights and anything copied to the clipboard stay. This can't be undone.
      </p>
      {#if clearFailed}
        <p class="confirm-error">Couldn't clear your history. Try again in a moment.</p>
      {/if}
      <div class="confirm-actions">
        <button class="btn-secondary" disabled={clearing} onclick={() => (confirmingClear = false)}>
          Cancel
        </button>
        <button class="btn-danger" disabled={clearing} onclick={clearAll}>
          {clearing ? "Clearing…" : "Clear all history"}
        </button>
      </div>
    </div>
  </div>
{/if}

<style>
  .history-page {
    max-width: 760px;
  }

  .retention-banner {
    display: flex;
    align-items: flex-start;
    gap: 10px;
    background: var(--caution-soft);
    border: 1px solid var(--caution-line);
    color: var(--caution);
    border-radius: var(--radius-card);
    padding: 12px 16px;
    font-size: 13px;
    line-height: 1.55;
    margin: -8px 0 20px;
  }

  .retention-banner :global(svg) {
    flex: none;
    margin-top: 1px;
  }

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

  .clear-all {
    flex: none;
    font-family: var(--font-ui);
    font-size: 12.5px;
    font-weight: 600;
    color: var(--fg-muted);
    background: transparent;
    border: 1px solid var(--hairline-strong);
    border-radius: var(--radius-control);
    padding: 7px 12px;
    cursor: pointer;
    white-space: nowrap;
    transition: color var(--motion), background var(--motion), border-color var(--motion);
  }

  .clear-all:hover {
    color: var(--danger);
    background: var(--danger-soft);
    border-color: transparent;
  }

  .loading-line {
    color: var(--fg-faint);
    font-size: 13px;
    padding: 20px 0;
  }

  .groups {
    display: flex;
    flex-direction: column;
    gap: 22px;
  }

  .group-label {
    font-size: 11px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.08em;
    color: var(--fg-faint);
    margin: 0 0 8px 2px;
  }

  ul {
    list-style: none;
    margin: 0;
    padding: 0;
    border: 1px solid var(--hairline);
    border-radius: var(--radius-card);
    background: var(--surface);
    overflow: hidden;
  }

  li {
    display: flex;
    align-items: center;
    gap: 6px;
    border-top: 1px solid var(--hairline);
    position: relative;
  }

  li:first-child {
    border-top: none;
  }

  .row {
    flex: 1;
    min-width: 0;
    display: flex;
    align-items: flex-start;
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

  .row .time {
    flex: none;
    width: 62px;
    font-size: 12.5px;
    color: var(--fg-faint);
    padding-top: 2px;
  }

  .row .text {
    flex: 1;
    min-width: 0;
    line-height: 1.55;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .row .meta {
    flex: none;
    display: flex;
    gap: 6px;
    padding-top: 2px;
  }

  .chip {
    font-size: 11px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.03em;
    color: var(--fg-muted);
    background: var(--bg-elevated);
    border: 1px solid var(--hairline);
    border-radius: 999px;
    padding: 3px 9px;
    white-space: nowrap;
  }

  .chip.provider {
    color: var(--teal);
    background: var(--teal-soft);
    border-color: transparent;
  }

  .chip.failed {
    color: var(--danger);
    background: var(--danger-soft);
    border-color: transparent;
  }

  .row-actions {
    flex: none;
    display: flex;
    gap: 2px;
    padding-right: 12px;
    opacity: 0;
    transition: opacity var(--motion);
  }

  li:hover .row-actions,
  li:focus-within .row-actions {
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
    background: var(--wash-strong);
    color: var(--fg);
  }

  .icon-btn.done {
    color: var(--teal);
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

  /* ---- Drawer ---- */
  .scrim {
    position: fixed;
    inset: 0;
    z-index: 60;
    background: var(--scrim);
    display: flex;
    justify-content: flex-end;
  }

  .drawer {
    width: min(480px, 100vw);
    height: 100vh;
    background: var(--surface);
    border-left: 1px solid var(--hairline);
    box-shadow: var(--shadow-drawer);
    padding: 26px 28px 24px;
    overflow-y: auto;
    display: flex;
    flex-direction: column;
    gap: 16px;
  }

  .drawer-head {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: 12px;
  }

  .drawer-date {
    font-size: 13px;
    font-weight: 600;
    color: var(--fg-muted);
    margin: 0 0 8px;
  }

  .drawer-chips {
    display: flex;
    gap: 6px;
    flex-wrap: wrap;
  }

  .drawer-error {
    color: var(--danger);
    background: var(--danger-soft);
    border-radius: var(--radius-control);
    padding: 10px 14px;
    font-size: 13px;
    margin: 0;
  }

  .no-edit {
    font-size: 12.5px;
    color: var(--teal);
    margin: 0;
  }

  .diff-text {
    font-size: 14px;
    line-height: 1.65;
    color: var(--fg);
    white-space: pre-wrap;
    user-select: text;
    margin: 0;
  }

  .diff-text.standalone {
    background: var(--bg-elevated);
    border: 1px solid var(--hairline);
    border-radius: var(--radius-card);
    padding: 14px 16px;
  }

  .diff {
    display: flex;
    flex-direction: column;
    gap: 14px;
  }

  .diff-pane {
    background: var(--bg-elevated);
    border: 1px solid var(--hairline);
    border-radius: var(--radius-card);
    padding: 14px 16px;
  }

  .diff-label {
    font-size: 11px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: var(--fg-faint);
    margin: 0 0 8px;
  }

  .drawer-actions {
    margin-top: auto;
    display: flex;
    gap: 10px;
    padding-top: 16px;
    border-top: 1px solid var(--hairline);
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
    background: var(--danger-soft);
    border: 1px solid transparent;
    color: var(--danger);
  }

  .btn-danger:hover {
    opacity: 0.85;
  }

  .btn-secondary:disabled,
  .btn-danger:disabled {
    opacity: 0.5;
    cursor: default;
  }

  /* ---- Clear all confirmation ---- */
  .confirm-scrim {
    justify-content: center;
    align-items: center;
    padding: 24px;
  }

  .confirm {
    width: min(420px, 100%);
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
  }

  .confirm-body {
    font-size: 13px;
    line-height: 1.55;
    color: var(--fg-muted);
    margin: 0 0 20px;
  }

  .confirm-error {
    font-size: 13px;
    color: var(--danger);
    margin: -8px 0 16px;
  }

  .confirm-actions {
    display: flex;
    justify-content: flex-end;
    gap: 10px;
  }
</style>
