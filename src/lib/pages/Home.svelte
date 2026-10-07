<script lang="ts">
  import { listen } from "@tauri-apps/api/event";
  import { onMount } from "svelte";
  import {
    STATE_CHANGED,
    NOTICE_ERROR,
    TRANSCRIPT_FINAL,
    type DictationState,
    type NoticePayload,
    type StatePayload,
  } from "$lib/events";
  import { historyList, historyUpdateText } from "$lib/api";
  import EmptyState from "$lib/components/EmptyState.svelte";
  import Icon from "$lib/components/Icon.svelte";
  import { todayFromHistory, todayOnly, type HomeItem } from "$lib/dictationData";
  import { learnPairs } from "$lib/learn";
  import { stats } from "$lib/stats.svelte";
  import { settings } from "$lib/stores.svelte";

  let dictation = $state<DictationState>("idle");
  let mode = $state<string | null>(null);
  let notice = $state("");
  let copied = $state<number | null>(null);
  let editingKey = $state<string | null>(null);
  let draft = $state("");
  let learnedNote = $state<{ key: string; text: string } | null>(null);
  let learnedTimer: ReturnType<typeof setTimeout> | undefined;

  /** The most rows Home asks the history database for; the list shows
   * today's, and the old in-webview copy kept 200 as well. */
  const TODAY_LIMIT = 200;

  const stateLabels: Record<DictationState, string> = {
    idle: "Ready",
    recording: "Listening…",
    finalizing: "Transcribing…",
    injecting: "Typing…",
  };

  let binding = $derived(settings.current?.hotkey.binding ?? "Ctrl+Win");
  let keys = $derived(binding.split("+").map((k) => k.trim()));
  /** A cloud engine — the user's own Sarvam key, or Butterfly Labs' relay.
   * The same spelling GeneralSection and CleanupSection use, so a fourth
   * provider has to be added to all three or to none. */
  let cloud = $derived(
    settings.current?.provider === "sarvam" || settings.current?.provider === "cloud",
  );
  let relay = $derived(settings.current?.provider === "cloud");
  /** The custom endpoint's speech-to-text switch outranks the provider: with
   * it on, every dictation goes to that server, whatever the provider is. */
  let ownStt = $derived(settings.current?.customEndpoint.useForStt === true);
  /** Today's dictations come from the one place their text is kept: the
   * history database while history is on, and this session's memory while
   * it is off (nothing is written then). See dictationData.ts. */
  let historyOn = $derived(settings.current?.history.enabled ?? true);
  let stored = $state<HomeItem[]>([]);
  let history = $derived(historyOn ? stored : todayOnly(stats.session, Date.now()));

  let loadSeq = 0;
  async function loadToday() {
    const seq = ++loadSeq;
    try {
      const rows = await historyList(0, TODAY_LIMIT);
      if (seq === loadSeq) stored = todayFromHistory(rows, Date.now());
    } catch {
      if (seq === loadSeq) stored = [];
    }
  }

  // On mount, and again whenever history is turned back on.
  $effect(() => {
    if (historyOn) loadToday();
  });

  onMount(() => {
    const unsubs: Array<() => void> = [];
    // The backend files the history row before it emits this event, on the
    // same DB thread queue a List then goes through, so the reload sees it.
    listen(TRANSCRIPT_FINAL, () => {
      if (historyOn) loadToday();
    }).then((u) => unsubs.push(u));
    listen<StatePayload>(STATE_CHANGED, (e) => {
      dictation = e.payload.state;
      mode = e.payload.mode;
      if (dictation === "recording") notice = "";
    }).then((u) => unsubs.push(u));
    listen<NoticePayload>(NOTICE_ERROR, (e) => {
      notice = e.payload.message;
    }).then((u) => unsubs.push(u));
    return () => unsubs.forEach((u) => u());
  });

  function fmtTime(at: number): string {
    return new Date(at)
      .toLocaleTimeString([], { hour: "numeric", minute: "2-digit" })
      .toLowerCase();
  }

  async function copy(text: string, i: number) {
    try {
      await navigator.clipboard.writeText(text);
      copied = i;
      setTimeout(() => (copied = null), 1200);
    } catch {
      /* clipboard unavailable — ignore */
    }
  }

  function startEdit(item: HomeItem) {
    editingKey = item.key;
    draft = item.text;
  }

  function cancelEdit() {
    editingKey = null;
    draft = "";
  }

  async function saveEdit(item: HomeItem) {
    const original = item.text;
    const edited = draft;
    editingKey = null;
    draft = "";
    if (!edited.trim() || edited === original) return;
    // The edit goes to wherever the text is kept: the history row, or this
    // session's memory while history is off.
    if (item.id !== null) {
      // `false` means the row is gone (deleted in History, or swept). A
      // rejected call says nothing about the row, so it gets its own notice,
      // and the editor comes back with the user's words in it so "try
      // again" does not mean retyping them.
      let saved: boolean;
      try {
        saved = await historyUpdateText(item.id, edited);
      } catch {
        notice = "Couldn't save the edit. Try again in a moment.";
        if (editingKey === null) {
          editingKey = item.key;
          draft = edited;
        }
        return;
      }
      if (!saved) {
        notice = "That dictation is no longer in your history, so the edit wasn't saved.";
        await loadToday();
        return;
      }
      stored = stored.map((i) => (i.key === item.key ? { ...i, text: edited } : i));
    } else {
      stats.editSession(item.key, edited);
    }
    stats.countEdit(item.at, original, edited);
    const pairs = learnPairs(original, edited, {
      existingReplacementFroms: (settings.current?.replacements ?? []).map((r) => r.from),
      dictionary: settings.current?.dictionary ?? [],
    });
    if (pairs.length === 0) return;
    await settings.update((st) => {
      st.replacements = [...st.replacements, ...pairs.map((p) => ({ ...p, auto: true }))];
    });
    if (learnedTimer) clearTimeout(learnedTimer);
    learnedNote = {
      key: item.key,
      text: "Learned: " + pairs.map((p) => `"${p.from}" → "${p.to}"`).join(", "),
    };
    learnedTimer = setTimeout(() => (learnedNote = null), 4000);
  }
</script>

<div class="dictation">
  <h1 class="hero">
    Speak anywhere with
    {#each keys as key, i}
      {#if i > 0}<span class="plus">+</span>{/if}<kbd>{key}</kbd>
    {/each}
  </h1>

  <div class="status-row">
    <span class="status" class:live={dictation !== "idle"}>
      <span class="dot" data-state={dictation}></span>
      {stateLabels[dictation]}{mode === "handsFree" ? " · hands-free" : ""}
    </span>
    <span class="hint">
      Hold to dictate · double-tap for hands-free · <kbd class="mini">Esc</kbd> cancels
    </span>
  </div>

  {#if notice}
    <p class="notice">{notice}</p>
  {/if}

  <div class="columns">
    <section class="feed">
      <p class="section-label">Today</p>
      {#if history.length === 0}
        <EmptyState
          icon="mic"
          title="Dictations you make today will show up here"
          body={`Click into any app, hold ${binding}, and start talking. Your words land wherever your cursor is.`}
        />
      {:else}
        <ul>
          {#each history as item, i (item.key)}
            <li>
              <span class="time">{fmtTime(item.at)}</span>
              {#if editingKey === item.key}
                <div class="editor">
                  <textarea class="edit-area" rows="3" autocomplete="off" bind:value={draft}
                  ></textarea>
                  <div class="edit-actions">
                    <button class="btn-save" onclick={() => saveEdit(item)}>Save</button>
                    <button class="btn-cancel" onclick={cancelEdit}>Cancel</button>
                  </div>
                  {#if item.id !== null}
                    <!-- The edit replaces the row's text only. `raw_text`, the
                         verbatim transcript, stays searchable in History. -->
                    <p class="edit-hint">
                      History keeps the original transcript too. To remove its wording, delete
                      the dictation in History.
                    </p>
                  {/if}
                </div>
              {:else}
                <div class="text">
                  {item.text}
                  {#if learnedNote && learnedNote.key === item.key}
                    <p class="learned">{learnedNote.text}</p>
                  {/if}
                </div>
                <button class="edit" onclick={() => startEdit(item)}>Edit</button>
                <button
                  class="copy"
                  class:done={copied === i}
                  aria-label="Copy"
                  onclick={() => copy(item.text, i)}
                >
                  <Icon name="copy" size={15} stroke={1.6} />
                </button>
              {/if}
            </li>
          {/each}
        </ul>
      {/if}
    </section>

    <aside class="rail">
      <div class="stats">
        <div class="stat">
          <span class="num">{stats.totalWords.toLocaleString()}</span>
          <span class="stat-label">total words</span>
        </div>
        {#if stats.avgWpm > 0}
          <div class="stat">
            <span class="num">{stats.avgWpm}</span>
            <span class="stat-label">wpm</span>
          </div>
        {/if}
        <div class="stat">
          <span class="num">{stats.streak}</span>
          <span class="stat-label">day streak</span>
        </div>
      </div>
      <div class="rail-card">
        <p class="rail-title">
          {#if ownStt}
            Your own endpoint
          {:else if relay}
            Butterfly Labs Cloud
          {:else if cloud}
            Powered by Sarvam AI
          {:else}
            On-device mode
          {/if}
        </p>
        <p class="rail-body">
          {#if ownStt}
            Dictation audio goes to the server you set under Custom endpoint, which
            transcribes it.
          {:else if cloud}
            English and 22 Indian languages, transcribed live as you speak.
          {:else}
            Speech is recognised on this machine, unless you import a recording with a
            Sarvam key saved.
          {/if}
        </p>
      </div>
    </aside>
  </div>
</div>

<style>
  .dictation {
    max-width: 980px;
    margin: 0 auto;
  }

  .hero {
    font-size: 27px;
    font-weight: 650;
    letter-spacing: -0.02em;
    line-height: 1.35;
    margin: 4px 0 14px;
  }

  .hero kbd {
    font-size: 22px;
    border-radius: 10px;
    padding: 0.08em 0.5em;
    margin: 0 2px;
  }

  .plus {
    font-weight: 600;
    color: var(--fg-muted);
    margin: 0 6px;
  }

  .status-row {
    display: flex;
    align-items: center;
    gap: 14px;
    flex-wrap: wrap;
    margin-bottom: 30px;
  }

  .status {
    display: inline-flex;
    align-items: center;
    gap: 8px;
    font-size: 13px;
    font-weight: 550;
    border: 1px solid var(--hairline);
    border-radius: var(--radius-pill);
    background: var(--bg-elevated);
    padding: 6px 14px;
    transition: border-color var(--motion);
  }

  .status.live {
    border-color: var(--hairline-strong);
  }

  .dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--fg-faint);
    transition: background var(--motion);
  }

  .dot[data-state="recording"] {
    background: var(--danger);
    animation: pulse 1.2s ease-in-out infinite;
  }

  .dot[data-state="finalizing"],
  .dot[data-state="injecting"] {
    background: var(--teal);
  }

  @keyframes pulse {
    50% {
      opacity: 0.4;
    }
  }

  .hint {
    font-size: 12.5px;
    color: var(--fg-faint);
  }

  kbd.mini {
    font-size: 11px;
    border-radius: 5px;
    padding: 1px 6px;
    background: var(--surface);
    border-color: var(--hairline-strong);
    color: var(--fg-muted);
    font-weight: 600;
  }

  .notice {
    color: var(--danger);
    font-size: 13px;
    background: var(--danger-soft);
    border-radius: var(--radius-control);
    padding: 10px 14px;
    margin: -14px 0 24px;
  }

  .columns {
    display: flex;
    gap: 28px;
    align-items: flex-start;
  }

  .feed {
    flex: 1;
    min-width: 0;
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
    align-items: flex-start;
    gap: 16px;
    padding: 16px 18px;
    border-top: 1px solid var(--hairline);
    position: relative;
  }

  li:first-child {
    border-top: none;
  }

  .time {
    flex: none;
    width: 62px;
    font-size: 12.5px;
    color: var(--fg-faint);
    padding-top: 2px;
  }

  .text {
    flex: 1;
    min-width: 0;
    line-height: 1.6;
    user-select: text;
    cursor: text;
  }

  .copy,
  .edit {
    flex: none;
    opacity: 0;
    border: none;
    background: transparent;
    color: var(--fg-faint);
    padding: 4px;
    border-radius: 6px;
    cursor: pointer;
    font-family: var(--font-ui);
    transition: opacity var(--motion), color var(--motion), background var(--motion);
  }

  .edit {
    font-size: 12px;
    font-weight: 600;
    padding: 4px 6px;
  }

  li:hover .copy,
  li:hover .edit,
  li:focus-within .copy,
  li:focus-within .edit {
    opacity: 1;
  }

  .copy:hover,
  .edit:hover {
    background: var(--wash);
    color: var(--fg);
  }

  .copy.done {
    opacity: 1;
    color: var(--teal);
  }

  .learned {
    margin: 6px 0 0;
    font-size: 13px;
    color: var(--teal);
  }

  .editor {
    flex: 1;
    min-width: 0;
  }

  .edit-area {
    display: block;
    width: 100%;
    box-sizing: border-box;
    background: var(--surface);
    border: 1px solid var(--hairline-strong);
    border-radius: var(--radius-control);
    padding: 8px 12px;
    font-family: var(--font-ui);
    font-size: 13px;
    line-height: 1.6;
    color: var(--fg);
    resize: vertical;
    user-select: text;
  }

  .edit-actions {
    display: flex;
    gap: 8px;
    margin-top: 8px;
  }

  .edit-hint {
    font-size: 12px;
    line-height: 1.5;
    color: var(--fg-faint);
    margin: 8px 0 0;
  }

  .btn-save,
  .btn-cancel {
    border-radius: var(--radius-control);
    font-family: var(--font-ui);
    font-size: 12.5px;
    font-weight: 600;
    padding: 6px 14px;
    cursor: pointer;
  }

  .btn-save {
    background: var(--accent);
    color: var(--accent-fg);
    border: 1px solid var(--accent);
  }

  .btn-cancel {
    background: transparent;
    border: 1px solid var(--hairline);
    color: var(--fg);
  }

  .rail {
    flex: none;
    width: 250px;
    display: flex;
    flex-direction: column;
    gap: 16px;
  }

  .stats {
    border: 1px solid var(--hairline);
    border-radius: var(--radius-card);
    background: var(--bg-elevated);
    padding: 22px 22px 10px;
  }

  .stat {
    display: flex;
    align-items: baseline;
    gap: 10px;
    margin-bottom: 14px;
  }

  .num {
    font-family: var(--font-display);
    font-size: 30px;
    line-height: 1;
    letter-spacing: -0.01em;
  }

  .stat-label {
    font-size: 14px;
    color: var(--fg-muted);
  }

  .rail-card {
    border: 1px solid var(--hairline);
    border-radius: var(--radius-card);
    background: var(--bg-elevated);
    padding: 18px 20px;
  }

  .rail-title {
    font-size: 14px;
    font-weight: 650;
    margin: 0 0 6px;
  }

  .rail-body {
    font-size: 12.5px;
    line-height: 1.55;
    color: var(--fg-muted);
    margin: 0;
  }
</style>
