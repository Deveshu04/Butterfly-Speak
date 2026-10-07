<script lang="ts">
  import { listen } from "@tauri-apps/api/event";
  import { onMount } from "svelte";
  import {
    importCancel,
    importClear,
    importPickFiles,
    importStart,
    importStatus,
  } from "$lib/api";
  import {
    IMPORT_DROP_HOVER,
    IMPORT_PROGRESS,
    type ImportDropHoverPayload,
    type ImportItemPayload,
    type ImportItemState,
    type ImportProgressPayload,
  } from "$lib/events";
  import EmptyState from "$lib/components/EmptyState.svelte";
  import Icon from "$lib/components/Icon.svelte";

  // The formats media::probe can actually verify. Shown, not enforced: the
  // picker's own filter and the probe both come from ACCEPTED_EXTENSIONS on
  // the Rust side, so this string is a label and never a second source of
  // truth that could drift.
  const FORMATS = "WAV, MP3, M4A, MP4, AAC, FLAC, OGG, OGA, WebM";

  // Opus is still accepted by the picker and still cannot be transcribed:
  // every import is decoded to 16 kHz mono WAV first (Sarvam's batch endpoint
  // silently returns nothing for anything else) and there is no pure-Rust
  // Opus decoder to do it with. The extension list cannot express this — an
  // .ogg or .webm may be Vorbis, which works, or Opus, which does not — so
  // the caveat is said here rather than by striking a format from the line
  // above. See media::probe::ACCEPTED_EXTENSIONS.
  const OPUS_NOTE = "Opus audio can't be transcribed — including most WebM recorded in a browser. Convert those to WAV first.";

  // A function, not a constant: `$state` deep-proxies the value it is given, so
  // seeding the rune from a shared object would put that object behind the
  // proxy — and the first write to `queue.items` would mutate the thing every
  // later mount starts from.
  const emptyQueue = (): ImportProgressPayload => ({
    running: false,
    total: 0,
    done: 0,
    failed: 0,
    cancelled: 0,
    percent: 0,
    items: [],
  });

  let queue = $state<ImportProgressPayload>(emptyQueue());
  let dropHover = $state(false);
  let error = $state<string | null>(null);
  let picking = $state(false);

  let queued = $derived(queue.items.filter((i) => i.state === "queued").length);
  let settled = $derived(queue.done + queue.failed + queue.cancelled);
  let canStart = $derived(!queue.running && queued > 0);

  const LABELS: Record<ImportItemState, string> = {
    queued: "Waiting",
    probing: "Checking",
    // Not "Preparing": the wait is long enough on a big recording that the
    // row has to say what is taking the time, and converting is a thing a
    // person can recognise as finite.
    converting: "Converting",
    uploading: "Uploading",
    transcribing: "Transcribing",
    done: "Imported",
    failed: "Failed",
    cancelled: "Cancelled",
  };

  const BUSY: ReadonlySet<ImportItemState> = new Set<ImportItemState>([
    "probing",
    "converting",
    "uploading",
    "transcribing",
  ]);

  function isBusy(state: ImportItemState): boolean {
    return BUSY.has(state);
  }

  async function pick() {
    error = null;
    picking = true;
    try {
      await importPickFiles();
    } catch (e) {
      error = String(e);
    } finally {
      picking = false;
    }
  }

  async function start() {
    error = null;
    try {
      await importStart();
    } catch (e) {
      // The Rust side writes these to be shown as-is — a missing API key is
      // the common one.
      error = String(e);
    }
  }

  async function cancel() {
    await importCancel();
  }

  async function clear() {
    error = null;
    await importClear();
  }

  onMount(() => {
    let disposed = false;
    let sawEvent = false;
    const offs: Array<() => void> = [];

    // A listener that resolves after the page is gone has to be taken straight
    // back down: the cleanup below has already run by then, so anything kept
    // here would outlive the page and pile up one handler per Notes↔Import
    // round trip.
    function keep(off: () => void) {
      if (disposed) off();
      else offs.push(off);
    }

    // Listen first, ask second — and each step on its own, so one that fails
    // cannot orphan a listener the next one would have kept.
    (async () => {
      try {
        keep(
          await listen<ImportProgressPayload>(IMPORT_PROGRESS, (e) => {
            sawEvent = true;
            queue = e.payload;
          }),
        );
      } catch {
        // Without the listener the page shows whatever the snapshot says and
        // stops there, which is better than showing nothing.
      }
      try {
        keep(
          await listen<ImportDropHoverPayload>(IMPORT_DROP_HOVER, (e) => {
            dropHover = e.payload.over;
          }),
        );
      } catch {
        // Only the drop highlight; the drop itself is Rust's and still works.
      }
      try {
        // The queue outlives this page, so a mount mid-run has to ask for a
        // snapshot rather than wait for the next state change — but a snapshot
        // taken before an event that lands while it is in flight is *older*
        // than the page's state, and applying it would roll the page backwards.
        // If the run ended in that gap no further event arrives to correct it,
        // leaving a stuck bar, a live Cancel button, and Transcribe and Clear
        // both out of reach (all three are gated on `queue.running`).
        // Registering first means no event can be missed; ignoring the snapshot
        // once one has landed means it can only ever be the starting point.
        const snapshot = await importStatus();
        if (!disposed && !sawEvent) queue = snapshot;
      } catch {
        // A queue that cannot be read leaves the page empty rather than wrong.
      }
    })();

    return () => {
      disposed = true;
      for (const off of offs) off();
    };
  });
</script>

{#snippet row(item: ImportItemPayload)}
  <li class="item" class:failed={item.state === "failed"}>
    <div class="item-icon" class:busy={isBusy(item.state)}>
      {#if item.state === "done"}
        <Icon name="check" size={15} />
      {:else if item.state === "failed"}
        <Icon name="close" size={15} />
      {:else}
        <Icon name="upload" size={15} />
      {/if}
    </div>
    <div class="item-body">
      <span class="item-name">{item.name}</span>
      {#if item.error}
        <span class="item-error">{item.error}</span>
      {:else if item.detail}
        <span class="item-detail">{item.detail}</span>
      {/if}
    </div>
    <span class="item-state" class:busy={isBusy(item.state)}>{LABELS[item.state]}</span>
  </li>
{/snippet}

<div class="page">
  <h1 class="page-title">Import</h1>
  <p class="page-desc">
    Turn a recording you already have into a note — transcribed in the cloud by Sarvam,
    with timings you can scroll through.
  </p>

  <!-- Not a real HTML drop target: with the OS-level file drop enabled, the
       webview never sees dragover/drop on Windows. Rust handles the drop and
       tells this panel when to light up, which is also why the page never
       learns a path. -->
  <div class="dropzone" class:hover={dropHover}>
    <div class="dz-glyph"><Icon name="upload" size={22} stroke={1.5} /></div>
    <p class="dz-title">Drop recordings anywhere in this window</p>
    <p class="dz-sub">{FORMATS}</p>
    <p class="dz-note">{OPUS_NOTE}</p>
    <button class="primary" onclick={pick} disabled={picking}>
      {picking ? "Choosing…" : "Choose files"}
    </button>
  </div>

  {#if error}
    <p class="error" role="alert">{error}</p>
  {/if}

  {#if queue.total === 0}
    <EmptyState
      icon="upload"
      title="Nothing queued"
      body="Pick a recording or drop one on the window. Nothing is uploaded until you press Transcribe."
    />
  {:else}
    <div class="bar-row">
      <div class="bar" role="progressbar" aria-valuenow={queue.percent} aria-valuemin={0} aria-valuemax={100}>
        <div class="bar-fill" style="width: {queue.percent}%"></div>
      </div>
      <span class="counts">
        {settled} of {queue.total}
        {#if queue.failed > 0}· {queue.failed} failed{/if}
      </span>
    </div>

    <div class="actions">
      {#if queue.running}
        <button class="primary" onclick={cancel}>Cancel</button>
        <!-- Sarvam has no cancel endpoint. Stopping here stops Butterfly
             Speak waiting; it does not stop — or refund — a job the service
             has already begun. Saying so is cheaper than a support ticket. -->
        <span class="cancel-note">
          Stops Butterfly Speak waiting. Sarvam may still finish, and still charge for,
          a recording it has already started.
        </span>
      {:else}
        <button class="primary" onclick={start} disabled={!canStart}>
          Transcribe {queued > 0 ? queued : ""}
        </button>
        <button class="ghost" onclick={clear}>Clear</button>
      {/if}
    </div>

    <ul class="item-list">
      {#each queue.items as item (item.id)}
        {@render row(item)}
      {/each}
    </ul>
  {/if}
</div>

<style>
  .dropzone {
    border: 1.5px dashed var(--hairline-strong);
    border-radius: var(--radius-card);
    background: var(--bg-elevated);
    padding: 30px 24px 26px;
    margin-bottom: 22px;
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 6px;
    text-align: center;
    transition: background var(--motion), border-color var(--motion);
  }

  .dropzone.hover {
    border-color: var(--accent);
    background: var(--wash);
  }

  .dz-glyph {
    width: 42px;
    height: 42px;
    display: grid;
    place-items: center;
    border-radius: 50%;
    background: var(--wash);
    color: var(--fg-muted);
    margin-bottom: 4px;
  }

  .dz-title {
    font-size: 14.5px;
    font-weight: 600;
    margin: 0;
  }

  .dz-sub {
    font-size: 12px;
    color: var(--fg-faint);
    margin: 0 0 4px;
  }

  .dz-note {
    font-size: 11px;
    line-height: 1.4;
    color: var(--fg-faint);
    max-width: 46ch;
    margin: 0 0 12px;
  }

  .error {
    font-size: 13px;
    line-height: 1.5;
    color: var(--danger, #c0392b);
    background: var(--wash);
    border-radius: var(--radius-control);
    padding: 10px 14px;
    margin: 0 0 18px;
  }

  .bar-row {
    display: flex;
    align-items: center;
    gap: 12px;
    margin-bottom: 14px;
  }

  .bar {
    flex: 1;
    height: 6px;
    border-radius: 3px;
    background: var(--wash);
    overflow: hidden;
  }

  .bar-fill {
    height: 100%;
    background: var(--accent);
    transition: width var(--motion);
  }

  .counts {
    font-size: 12.5px;
    color: var(--fg-muted);
    white-space: nowrap;
  }

  .actions {
    display: flex;
    gap: 10px;
    align-items: center;
    margin-bottom: 18px;
  }

  button.primary {
    font-family: var(--font-ui);
    font-size: 13px;
    font-weight: 600;
    border: none;
    border-radius: var(--radius-control);
    background: var(--accent);
    color: var(--accent-fg);
    padding: 8px 18px;
    cursor: pointer;
  }

  button.primary:disabled {
    opacity: 0.5;
    cursor: default;
  }

  button.ghost {
    font-family: var(--font-ui);
    font-size: 13px;
    font-weight: 500;
    border: 1px solid var(--hairline-strong);
    border-radius: var(--radius-control);
    background: var(--surface);
    color: var(--fg);
    padding: 8px 16px;
    cursor: pointer;
  }

  button.ghost:hover {
    background: var(--bg-elevated);
  }

  .item-list {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }

  .item {
    display: flex;
    align-items: center;
    gap: 12px;
    background: var(--bg-elevated);
    border: 1px solid var(--hairline);
    border-radius: var(--radius-card);
    padding: 11px 14px;
  }

  .item.failed {
    border-color: var(--hairline-strong);
  }

  .item-icon {
    width: 26px;
    height: 26px;
    flex: none;
    display: grid;
    place-items: center;
    border-radius: 50%;
    background: var(--wash);
    color: var(--fg-muted);
  }

  .item-icon.busy {
    color: var(--accent);
  }

  .item-body {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }

  .item-name {
    font-size: 13.5px;
    font-weight: 550;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }

  .item-error,
  .item-detail {
    font-size: 12px;
    line-height: 1.45;
    color: var(--fg-muted);
  }

  .item-detail {
    color: var(--fg-faint);
  }

  .cancel-note {
    align-self: center;
    font-size: 12px;
    line-height: 1.4;
    color: var(--fg-faint);
    max-width: 46ch;
  }

  .item-state {
    flex: none;
    font-size: 11.5px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    color: var(--fg-faint);
  }

  .item-state.busy {
    color: var(--accent);
  }
</style>
