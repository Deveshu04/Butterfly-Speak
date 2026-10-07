<script lang="ts">
  import { emit, listen } from "@tauri-apps/api/event";
  import { onDestroy, onMount } from "svelte";
  import { hotkeyCapture, type Settings } from "$lib/api";
  import { HOTKEY_CAPTURE, NAVIGATE, type HotkeyCapturePayload } from "$lib/events";
  import Icon from "$lib/components/Icon.svelte";
  import { settings, ui } from "$lib/stores.svelte";

  let { onclose }: { onclose: () => void } = $props();

  type RowId =
    | "pushToTalk"
    | "translateDictation"
    | "voiceAgent"
    | "pasteLast"
    | "copyLast"
    | "scratchpad"
    | "undoAiEdit";

  let s = $derived(settings.current);
  let capturing = $state<RowId | null>(null);
  // A click can be followed by a keypress faster than Tauri can complete the
  // invoke that arms native capture. Keep that short setup phase explicit so
  // no keypress is lost to the old global shortcut handler.
  let arming = $state<RowId | null>(null);
  let captureAttempt = 0;
  let captureLive = $state("");
  /** Why the last press was ignored, or why a save failed. */
  let notice = $state("");

  interface RowDef {
    id: RowId;
    title: string;
    desc: string;
    clearable: boolean;
    get: () => string;
    set: (v: string) => void;
  }

  /** Persist a chord, surfacing failures. A rejected save would otherwise
   * leave the row showing the old chord with the reason buried in the
   * console — indistinguishable from "recording did nothing". */
  function save(mutate: (s: Settings) => void) {
    settings.update(mutate).catch((err) => {
      notice = `Couldn't save that shortcut: ${err}`;
    });
  }

  const ROWS: RowDef[] = [
    {
      id: "pushToTalk",
      title: "Push to talk",
      desc: "Hold to dictate; release to type",
      clearable: false,
      get: () => settings.current?.hotkey.binding ?? "",
      set: (v) => save((st) => (st.hotkey.binding = v)),
    },
    {
      id: "translateDictation",
      title: "Translate dictation",
      desc: "Hold to dictate, then type it in your target language",
      clearable: true,
      get: () => settings.current?.shortcuts.translateDictation ?? "",
      set: (v) => save((st) => (st.shortcuts.translateDictation = v)),
    },
    {
      id: "voiceAgent",
      title: "Voice agent",
      desc: "Hold to speak a command for the agent instead of text to type",
      clearable: true,
      get: () => settings.current?.shortcuts.voiceAgent ?? "",
      set: (v) => save((st) => (st.shortcuts.voiceAgent = v)),
    },
    {
      id: "pasteLast",
      title: "Paste last transcript",
      desc: "Paste the last thing you dictated",
      clearable: true,
      get: () => settings.current?.shortcuts.pasteLast ?? "",
      set: (v) => save((st) => (st.shortcuts.pasteLast = v)),
    },
    {
      id: "copyLast",
      title: "Copy last transcript",
      desc: "Copy the last thing you dictated",
      clearable: true,
      get: () => settings.current?.shortcuts.copyLast ?? "",
      set: (v) => save((st) => (st.shortcuts.copyLast = v)),
    },
    {
      id: "undoAiEdit",
      title: "Undo AI edit",
      desc: "Swaps a recent AI edit for exactly what you said, when it's safe — otherwise copies it for you to paste",
      clearable: true,
      get: () => settings.current?.shortcuts.undoAiEdit ?? "",
      set: (v) => save((st) => (st.shortcuts.undoAiEdit = v)),
    },
    {
      // The row id and the settings field keep the Scratchpad's name — the
      // page it opens is now Notes, but renaming the stored field would drop
      // whatever binding the user already has.
      id: "scratchpad",
      title: "Open Notes",
      desc: "Bring up Butterfly Speak on the Notes page",
      clearable: true,
      get: () => settings.current?.shortcuts.scratchpad ?? "",
      set: (v) => save((st) => (st.shortcuts.scratchpad = v)),
    },
  ];

  /** The three chords that start a recording. They share one slot in the
   * keyboard hook — only one can be held at a time — so unlike the app
   * shortcuts below them, they can shadow each other. */
  const DICTATION_ROWS: RowId[] = ["pushToTalk", "translateDictation", "voiceAgent"];

  /** Does chord `a` shadow chord `b`? True when every key `a` needs is also
   * needed by `b`, so `b` can never be pressed without satisfying `a` first.
   * Mirrors `hotkeys::shadows`; keep the two in step. */
  function shadows(a: Set<string>, b: Set<string>): boolean {
    if (a.size === 0 || b.size === 0) return false;
    return [...a].every((k) => b.has(k));
  }

  /** The title of the dictation chord this one would collide with, if any.
   *
   * With push-to-talk on Ctrl+Win, a translate chord bound Ctrl+Win+T can
   * never fire: the hook satisfies the main binding the moment Ctrl and Win
   * are down and never sees the T. `hotkeys::route_chord_bindings` refuses to
   * register such a chord (settings can also arrive by import or by hand), so
   * saving it here would leave a row showing a shortcut that does nothing.
   * Say so instead. */
  function dictationChordClash(id: RowId, binding: string): string | null {
    if (!DICTATION_ROWS.includes(id)) return null;
    const candidate = new Set(chips(binding).map((k) => k.toLowerCase()));
    for (const other of DICTATION_ROWS) {
      if (other === id) continue;
      const row = ROWS.find((r) => r.id === other);
      const value = row?.get() ?? "";
      if (!value) continue;
      const existing = new Set(chips(value).map((k) => k.toLowerCase()));
      if (shadows(candidate, existing) || shadows(existing, candidate)) return row!.title;
    }
    return null;
  }

  onMount(() => {
    // Listener is live before any capture can start.
    let unsub: (() => void) | undefined;
    captureListenerReady = listen<HotkeyCapturePayload>(HOTKEY_CAPTURE, (e) => {
      if (!capturing) return;
      // A hint means the press was ignored on purpose (unnameable key, or a
      // chord too weak to bind). Recording continues, so say why rather than
      // sitting there looking frozen.
      if (e.payload.hint) {
        notice = e.payload.hint;
        return;
      }
      notice = "";
      captureLive = e.payload.keys;
      if (e.payload.done) {
        const target = ROWS.find((r) => r.id === capturing);
        stopCapture();
        // Empty = cancelled (Escape).
        if (!e.payload.keys || !target) return;
        const clash = dictationChordClash(target.id, e.payload.keys);
        if (clash) {
          notice = `${e.payload.keys} overlaps “${clash}” — one chord would swallow the other, so neither could be told apart. Pick keys that don't nest.`;
          return;
        }
        target.set(e.payload.keys);
      }
    }).then((u) => {
      unsub = u;
    });
    return () => unsub?.();
  });

  onDestroy(() => {
    ui.capturingShortcut = false;
    if (capturing || arming) hotkeyCapture(false);
  });

  let captureListenerReady: Promise<void> = Promise.resolve();

  async function startCapture(id: RowId) {
    if (capturing || arming) return;
    const attempt = ++captureAttempt;
    arming = id;
    captureLive = "";
    notice = "";
    ui.capturingShortcut = true;
    try {
      // The listener registration and native atomic flag must both be in
      // place before we tell someone to press a shortcut. With DevTools open
      // the extra delay hides this race; normal use does not.
      await captureListenerReady;
      if (arming !== id || captureAttempt !== attempt) return;
      await hotkeyCapture(true);
      if (arming !== id || captureAttempt !== attempt) {
        await hotkeyCapture(false);
        return;
      }
      capturing = id;
    } catch (err) {
      notice = `Couldn't start recording: ${err}`;
      ui.capturingShortcut = false;
    } finally {
      if (captureAttempt === attempt) arming = null;
    }
  }

  function stopCapture() {
    captureAttempt += 1;
    arming = null;
    capturing = null;
    captureLive = "";
    ui.capturingShortcut = false;
    hotkeyCapture(false);
  }

  function resetDefaults() {
    if (capturing || arming) stopCapture();
    notice = "";
    save((st) => {
      st.hotkey.binding = "Ctrl+Win";
      st.shortcuts.pasteLast = "Alt+Shift+Z";
      st.shortcuts.copyLast = "Alt+Shift+X";
      st.shortcuts.scratchpad = "";
      st.shortcuts.undoAiEdit = "";
      // Both dictation-grade chords ship unbound (settings.rs's
      // `ShortcutSettings::default`), so "reset" clears them.
      st.shortcuts.translateDictation = "";
      st.shortcuts.voiceAgent = "";
    });
  }

  function goTransforms() {
    emit(NAVIGATE, { page: "transforms" });
    onclose();
  }

  function chips(binding: string): string[] {
    return binding
      .split("+")
      .map((k) => k.trim())
      .filter(Boolean);
  }
</script>

<!-- Captured at the window and stopped there: Escape belongs to this dialog,
     and the Settings modal behind it, whose window listener was added first,
     must not close on the same key. While a chord is being recorded the
     keyboard hook ends the recording on Escape, so the dialog stays open. -->
<svelte:window
  onkeydowncapture={(e) => {
    if (e.key !== "Escape") return;
    e.stopPropagation();
    if (arming) stopCapture();
    else if (!capturing) onclose();
  }}
/>

<div
  class="scrim"
  role="presentation"
  onclick={(e) => {
    if (e.target === e.currentTarget) onclose();
  }}
>
  <div class="dialog" role="dialog" aria-modal="true" aria-label="Shortcuts">
    <header>
      <h2>Shortcuts</h2>
      <button class="close" aria-label="Close" onclick={onclose}>
        <Icon name="close" size={16} stroke={1.6} />
      </button>
    </header>
    <p class="sub">Choose your preferred shortcuts for Butterfly Speak.</p>

    {#if notice}
      <p class="notice" role="status">{notice}</p>
    {/if}

    <div class="rows">
      {#if s}
        {#each ROWS as row (row.id)}
          {@const value = row.get()}
          <div class="row">
            <div class="info">
              <p class="row-title">{row.title}</p>
              <p class="row-desc">{row.desc}</p>
            </div>
            <div class="pill" class:capturing={capturing === row.id || arming === row.id}>
              {#if capturing === row.id || arming === row.id}
                {#if arming === row.id}
                  <span class="placeholder">Preparing recorder…</span>
                {:else if captureLive}
                  {#each chips(captureLive) as key}<kbd class="chip">{key}</kbd>{/each}
                {:else}
                  <span class="placeholder">Press your combination…</span>
                {/if}
                <button class="mini" aria-label="Cancel" onclick={stopCapture}>
                  <Icon name="close" size={13} stroke={1.7} />
                </button>
              {:else}
                {#if value}
                  {#each chips(value) as key}<kbd class="chip">{key}</kbd>{/each}
                {:else}
                  <span class="placeholder">Click to add a shortcut</span>
                {/if}
                <button
                  class="mini"
                  aria-label="Change shortcut"
                  onclick={() => startCapture(row.id)}
                >
                  <Icon name="pencil" size={13} stroke={1.7} />
                </button>
                {#if row.clearable && value}
                  <button class="mini" aria-label="Remove shortcut" onclick={() => row.set("")}>
                    <Icon name="trash" size={13} stroke={1.7} />
                  </button>
                {/if}
              {/if}
            </div>
          </div>

          {#if row.id === "pushToTalk"}
            <div class="row">
              <div class="info">
                <p class="row-title">Hands-free mode</p>
                <p class="row-desc">
                  Dictate hands-free by double-tapping the hotkey; tap again to stop
                </p>
              </div>
              <div class="pill readonly">
                <span class="prefix">Double tap</span>
                {#each chips(s.hotkey.binding) as key}<kbd class="chip">{key}</kbd>{/each}
              </div>
            </div>
          {/if}
        {/each}

        <button class="row link" onclick={goTransforms}>
          <div class="info">
            <p class="row-title">Transform</p>
            <p class="row-desc">Configure Polish, custom rewrites, and more in the Transforms tab</p>
          </div>
          <Icon name="chevron-right" size={17} stroke={1.8} />
        </button>

        <div class="row">
          <div class="info">
            <p class="row-title">Cancel</p>
            <p class="row-desc">Dismiss dictation and notifications</p>
          </div>
          <div class="pill readonly">
            <kbd class="chip">esc</kbd>
          </div>
        </div>
      {/if}
    </div>

    <footer>
      <button class="soft" onclick={resetDefaults}>Reset to default</button>
      <button class="done" onclick={onclose}>Done</button>
    </footer>
  </div>
</div>

<style>
  .scrim {
    position: fixed;
    inset: 0;
    z-index: 70;
    background: var(--scrim);
    display: grid;
    place-items: center;
    animation: fade 130ms ease;
  }

  @keyframes fade {
    from {
      opacity: 0;
    }
  }

  .dialog {
    width: min(680px, calc(100vw - 96px));
    max-height: min(720px, calc(100vh - 96px));
    background: var(--bg-elevated);
    border-radius: var(--radius-panel);
    box-shadow: var(--shadow-modal);
    display: flex;
    flex-direction: column;
    padding: 26px 28px 20px;
    animation: rise 150ms ease;
  }

  @keyframes rise {
    from {
      opacity: 0;
      transform: translateY(6px) scale(0.99);
    }
  }

  header {
    display: flex;
    align-items: center;
    justify-content: space-between;
  }

  h2 {
    font-size: 20px;
    font-weight: 650;
    margin: 0;
  }

  .close {
    border: none;
    background: transparent;
    color: var(--fg-muted);
    padding: 6px;
    border-radius: 8px;
    cursor: pointer;
  }

  .close:hover {
    background: var(--wash);
    color: var(--fg);
  }

  .sub {
    font-size: 13.5px;
    color: var(--fg-muted);
    margin: 4px 0 18px;
  }

  .notice {
    font-size: 13px;
    color: var(--fg);
    background: var(--notice-bg);
    border: 1px solid var(--notice-line);
    border-radius: 10px;
    padding: 9px 12px;
    margin: 0 0 14px;
  }

  .rows {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    display: flex;
    flex-direction: column;
    gap: 10px;
    padding-bottom: 4px;
  }

  .row {
    display: flex;
    align-items: center;
    gap: 20px;
    background: var(--sunken);
    border-radius: var(--radius-card);
    padding: 18px 20px;
  }

  .row.link {
    border: none;
    font-family: var(--font-ui);
    text-align: left;
    cursor: pointer;
    color: var(--fg);
    transition: background var(--motion);
  }

  .row.link:hover {
    background: var(--chip);
  }

  .row.link :global(svg) {
    color: var(--fg-muted);
    flex: none;
  }

  .info {
    flex: 1;
    min-width: 0;
  }

  .row-title {
    font-size: 15px;
    font-weight: 600;
    margin: 0;
  }

  .row-desc {
    font-size: 13px;
    color: var(--fg-muted);
    margin: 3px 0 0;
    line-height: 1.45;
  }

  .pill {
    flex: none;
    display: flex;
    align-items: center;
    gap: 6px;
    background: var(--surface);
    border: 1px solid var(--hairline-strong);
    border-radius: 12px;
    padding: 9px 12px;
    min-width: 170px;
    min-height: 40px;
    transition: border-color var(--motion);
  }

  .pill.capturing {
    border-color: var(--teal);
    box-shadow: var(--focus-ring);
  }

  .pill.readonly {
    border-color: var(--hairline);
  }

  .chip {
    font-family: var(--font-ui);
    font-size: 12.5px;
    font-weight: 600;
    background: var(--chip);
    border: none;
    border-radius: 6px;
    padding: 3px 9px;
    color: var(--fg);
  }

  .prefix {
    font-size: 12.5px;
    color: var(--fg-muted);
    margin-right: 2px;
  }

  .placeholder {
    font-size: 13px;
    color: var(--fg-faint);
    flex: 1;
  }

  .mini {
    margin-left: auto;
    border: none;
    background: transparent;
    color: var(--fg-faint);
    padding: 4px;
    border-radius: 6px;
    cursor: pointer;
    display: grid;
    place-items: center;
  }

  .mini + .mini {
    margin-left: 0;
  }

  .mini:hover {
    background: var(--wash-strong);
    color: var(--fg);
  }

  footer {
    display: flex;
    justify-content: space-between;
    align-items: center;
    padding-top: 18px;
  }

  .soft {
    font-family: var(--font-ui);
    font-size: 13.5px;
    font-weight: 600;
    border: 1px solid var(--hairline);
    background: var(--chip);
    color: var(--fg);
    border-radius: 10px;
    padding: 10px 20px;
    cursor: pointer;
  }

  .done {
    font-family: var(--font-ui);
    font-size: 13.5px;
    font-weight: 600;
    border: 1px solid var(--accent);
    background: var(--accent);
    color: var(--accent-fg);
    border-radius: 10px;
    padding: 10px 26px;
    cursor: pointer;
  }
</style>
