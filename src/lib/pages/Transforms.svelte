<script lang="ts">
  import { listen } from "@tauri-apps/api/event";
  import { onDestroy, onMount } from "svelte";
  import { hotkeyCapture, type Transform } from "$lib/api";
  import { HOTKEY_CAPTURE, type HotkeyCapturePayload } from "$lib/events";
  import { settings, ui } from "$lib/stores.svelte";
  import Banner from "$lib/components/Banner.svelte";
  import EmptyState from "$lib/components/EmptyState.svelte";
  import Icon from "$lib/components/Icon.svelte";
  import Toggle from "$lib/components/Toggle.svelte";

  let s = $derived(settings.current);

  // Editor state. editIndex === -1 means "new transform".
  let editing = $state(false);
  let editIndex = $state(-1);
  let draftName = $state("");
  let draftPrompt = $state("");
  let draftShortcut = $state("");

  // Shortcut recording.
  let capturing = $state(false);
  let arming = $state(false);
  let captureAttempt = 0;
  let captureLive = $state("");
  /** Why the last press was ignored, when it was. */
  let captureHint = $state("");

  onMount(() => {
    let unsub: (() => void) | undefined;
    captureListenerReady = listen<HotkeyCapturePayload>(HOTKEY_CAPTURE, (e) => {
      // This page stays mounted underneath the settings modal, so it also sees
      // recordings started from the Shortcuts dialog. Only react while this
      // page is the one recording.
      if (!capturing) return;
      if (e.payload.hint) {
        captureHint = e.payload.hint;
        return;
      }
      captureHint = "";
      captureLive = e.payload.keys;
      if (e.payload.done) {
        capturing = false;
        ui.capturingShortcut = false;
        hotkeyCapture(false);
        if (e.payload.keys) draftShortcut = e.payload.keys;
      }
    }).then((u) => {
      unsub = u;
    });
    return () => unsub?.();
  });

  onDestroy(() => {
    if (capturing || arming) {
      ui.capturingShortcut = false;
      hotkeyCapture(false);
    }
  });

  let captureListenerReady: Promise<void> = Promise.resolve();

  /** `ui.capturingShortcut` is set for as long as the recorder is armed or
   * listening, the same as the Shortcuts dialog sets it: the keyboard hook
   * lets the chord through to this window too, so without it Ctrl+K would
   * open the command palette over the editor, and Escape would close
   * Settings if it were open, in the same press that is being recorded. */
  async function startCapture() {
    if (capturing || arming) return;
    const attempt = ++captureAttempt;
    arming = true;
    captureLive = "";
    captureHint = "";
    ui.capturingShortcut = true;
    try {
      await captureListenerReady;
      if (!arming || captureAttempt !== attempt) return;
      await hotkeyCapture(true);
      if (!arming || captureAttempt !== attempt) {
        await hotkeyCapture(false);
        return;
      }
      capturing = true;
    } catch (err) {
      captureHint = `Couldn't start recording: ${err}`;
      ui.capturingShortcut = false;
    } finally {
      if (captureAttempt === attempt) arming = false;
    }
  }

  function cancelCapture() {
    captureAttempt += 1;
    arming = false;
    capturing = false;
    captureHint = "";
    ui.capturingShortcut = false;
    hotkeyCapture(false);
  }

  function chips(shortcut: string): string[] {
    return shortcut
      .split("+")
      .map((k) => k.trim())
      .filter(Boolean);
  }

  function openEditor(index: number) {
    if (capturing) cancelCapture();
    const t = index >= 0 ? s?.transforms[index] : undefined;
    editIndex = index;
    draftName = t?.name ?? "";
    draftPrompt = t?.prompt ?? "";
    draftShortcut = t?.shortcut ?? "";
    editing = true;
  }

  function closeEditor() {
    if (capturing) cancelCapture();
    editing = false;
    editIndex = -1;
  }

  async function save() {
    const name = draftName.trim();
    const prompt = draftPrompt.trim();
    if (!name || !prompt) return;
    const entry: Transform = { name, prompt, shortcut: draftShortcut };
    const idx = editIndex;
    await settings.update((st) => {
      if (idx >= 0 && idx < st.transforms.length) st.transforms[idx] = entry;
      else st.transforms.push(entry);
    });
    closeEditor();
  }

  async function remove(index: number) {
    if (editing && editIndex === index) closeEditor();
    else if (editing && editIndex > index) editIndex -= 1;
    await settings.update((st) => {
      st.transforms.splice(index, 1);
    });
  }
</script>

{#snippet shortcutChips(shortcut: string)}
  {#if chips(shortcut).length > 0}
    {#each chips(shortcut) as key, ki}
      {#if ki > 0}<span class="chip-join" aria-hidden="true">+</span>{/if}
      <kbd class="chip">{key}</kbd>
    {/each}
  {:else}
    <span class="no-shortcut">No shortcut</span>
  {/if}
{/snippet}

{#if s}
  <div class="page">
    <div class="header-row">
      <h1 class="page-title">Transforms</h1>
      <div class="enabled-toggle">
        <Toggle
          label="Enabled"
          checked={s.transformsEnabled}
          onchange={(v) => settings.update((st) => (st.transformsEnabled = v))}
        />
      </div>
    </div>
    <p class="page-desc">
      Rewrite anything you've written — select text, press the shortcut.
    </p>

    <Banner
      motif="spark"
      body="Select text in any app, press the shortcut, and the rewrite lands in place."
    >
      Transform works <em>anywhere</em> you write.
    </Banner>

    <p class="section-label">My Transforms</p>

    {#if s.transforms.length === 0}
      <div class="empty-wrap">
        <EmptyState
          icon="sparkles"
          title="No transforms yet"
          body="Create one to rewrite selected text with a shortcut."
        >
          {#snippet action()}
            <button onclick={() => openEditor(-1)}>Create a transform</button>
          {/snippet}
        </EmptyState>
      </div>
    {:else}
      <div class="grid">
        {#each s.transforms as t, i (i)}
          <div class="card">
            <div class="card-actions">
              <button
                class="icon-btn"
                title="Edit"
                aria-label="Edit {t.name}"
                onclick={() => openEditor(i)}
              >
                <Icon name="pencil" size={15} stroke={1.6} />
              </button>
              <button
                class="icon-btn"
                title="Delete"
                aria-label="Delete {t.name}"
                onclick={() => remove(i)}
              >
                <Icon name="trash" size={15} stroke={1.6} />
              </button>
            </div>
            <div class="chips">
              {@render shortcutChips(t.shortcut)}
            </div>
            <p class="name">{t.name}</p>
            <p class="prompt">{t.prompt}</p>
          </div>
        {/each}

        <button class="card create" onclick={() => openEditor(-1)}>
          <span class="create-glyph" aria-hidden="true">
            <Icon name="plus" size={18} stroke={1.6} />
          </span>
          <span class="create-label">Create your own</span>
          <span class="create-sub">Name it, write the rule, pick a shortcut</span>
        </button>
      </div>
    {/if}

    {#if editing}
      <div class="editor">
        <p class="section-label editor-title">
          {editIndex >= 0 ? "Edit transform" : "New transform"}
        </p>

        <label class="field">
          <span class="field-label">Name</span>
          <input
            type="text"
            bind:value={draftName}
            placeholder="e.g. Make it formal"
          />
        </label>

        <label class="field">
          <span class="field-label">Prompt</span>
          <textarea
            rows={4}
            bind:value={draftPrompt}
            placeholder="Rewrite the selected text to…"
          ></textarea>
        </label>

        <div class="field">
          <span class="field-label">Shortcut</span>
          <div class="shortcut-row">
            {#if capturing || arming}
              <span class="live">
                {arming ? "Preparing recorder…" : captureHint || captureLive || "Press your combination…"}
              </span>
              <button class="ghost" onclick={cancelCapture}>Cancel</button>
            {:else}
              <span class="chips">
                {@render shortcutChips(draftShortcut)}
              </span>
              <button class="ghost" onclick={startCapture}>Record shortcut</button>
            {/if}
          </div>
        </div>

        <div class="editor-actions">
          <button class="ghost" onclick={closeEditor}>Cancel</button>
          <button
            class="primary"
            onclick={save}
            disabled={!draftName.trim() || !draftPrompt.trim()}
          >
            Save
          </button>
        </div>
      </div>
    {/if}

    <p class="foot-note">
      The selected text goes to the AI model your settings use for transforms: Sarvam
      AI, through the Butterfly Labs relay on Cloud, or your own AI endpoint when it is
      on for AI Polish. Works on up to 1,000 words of selected text.
    </p>
  </div>
{/if}

<style>
  .header-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 16px;
    margin-bottom: 24px;
  }

  .header-row h1 {
    margin: 0;
  }

  .enabled-toggle {
    flex: none;
  }

  .empty-wrap {
    margin-bottom: 24px;
  }

  .grid {
    display: flex;
    flex-wrap: wrap;
    gap: 14px;
    margin-bottom: 24px;
  }

  .card {
    box-sizing: border-box;
    position: relative;
    width: 300px;
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    background: var(--surface);
    border: 1px solid var(--hairline);
    border-radius: var(--radius-card);
    padding: 18px 20px;
  }

  .card-actions {
    position: absolute;
    top: 12px;
    right: 12px;
    display: flex;
    gap: 2px;
    opacity: 0;
    transition: opacity 120ms ease;
  }

  .card:hover .card-actions,
  .card:focus-within .card-actions {
    opacity: 1;
  }

  .icon-btn {
    width: 28px;
    height: 28px;
    display: grid;
    place-items: center;
    padding: 0;
    background: transparent;
    border: none;
    border-radius: 6px;
    color: var(--fg-faint);
    cursor: pointer;
  }

  .icon-btn:hover {
    background: var(--wash);
    color: var(--fg);
  }

  .chips {
    display: inline-flex;
    flex-wrap: wrap;
    gap: 5px;
    align-items: center;
    min-height: 28px;
  }

  .card .chips {
    padding-right: 56px;
  }

  kbd.chip {
    background: var(--surface);
    border: 1px solid var(--hairline-strong);
    border-radius: 7px;
    padding: 4px 10px;
    font-size: 13px;
    font-weight: 650;
    color: var(--fg);
    font-family: var(--font-ui);
  }

  .chip-join {
    color: var(--fg-faint);
    font-size: 12px;
  }

  .no-shortcut {
    color: var(--fg-faint);
    font-size: 12px;
  }

  .name {
    font-size: 16px;
    font-weight: 650;
    margin: 14px 0 0;
  }

  .prompt {
    color: var(--fg-muted);
    font-size: 13px;
    line-height: 1.5;
    margin: 6px 0 0;
    min-height: 39px;
    display: -webkit-box;
    -webkit-line-clamp: 2;
    line-clamp: 2;
    -webkit-box-orient: vertical;
    overflow: hidden;
  }

  .card.create {
    justify-content: center;
    align-items: center;
    border-style: dashed;
    border-color: var(--hairline-strong);
    background: transparent;
    cursor: pointer;
    min-height: 158px;
    font-family: var(--font-ui);
  }

  .create-glyph {
    width: 40px;
    height: 40px;
    border-radius: 50%;
    border: 1px solid var(--hairline-strong);
    background: var(--surface);
    display: grid;
    place-items: center;
    color: var(--fg-faint);
    margin-bottom: 10px;
  }

  .create-label {
    font-size: 14px;
    font-weight: 600;
    color: var(--fg);
  }

  .create-sub {
    margin-top: 3px;
    font-size: 12.5px;
    color: var(--fg-faint);
  }

  .editor {
    box-sizing: border-box;
    max-width: 560px;
    display: flex;
    flex-direction: column;
    gap: 14px;
    background: var(--surface);
    border: 1px solid var(--hairline);
    border-radius: var(--radius-card);
    padding: 20px;
    margin-bottom: 24px;
  }

  .editor-title {
    margin: 0;
  }

  .field {
    display: flex;
    flex-direction: column;
    gap: 6px;
  }

  .field-label {
    font-size: 13px;
    font-weight: 500;
    color: var(--fg-muted);
  }

  input,
  textarea {
    box-sizing: border-box;
    width: 100%;
    background: var(--surface);
    border: 1px solid var(--hairline-strong);
    border-radius: var(--radius-control);
    padding: 8px 12px;
    font-family: var(--font-ui);
    font-size: 13px;
    color: var(--fg);
  }

  textarea {
    resize: vertical;
    line-height: 1.5;
  }

  .shortcut-row {
    display: flex;
    align-items: center;
    gap: 12px;
  }

  .live {
    color: var(--fg-muted);
    font-size: 13px;
    border: 1px solid var(--hairline-strong);
    border-radius: var(--radius-control);
    background: var(--surface);
    padding: 8px 12px;
    min-width: 160px;
  }

  .editor-actions {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
    margin-top: 4px;
  }

  button.primary,
  button.ghost {
    font-family: var(--font-ui);
    font-size: 13px;
    font-weight: 600;
    border-radius: var(--radius-control);
    padding: 8px 16px;
    cursor: pointer;
  }

  button.primary {
    background: var(--accent);
    color: var(--accent-fg);
    border: 1px solid var(--accent);
  }

  button.primary:disabled {
    opacity: 0.45;
    cursor: default;
  }

  button.ghost {
    background: transparent;
    border: 1px solid var(--hairline);
    color: var(--fg);
  }

  .foot-note {
    color: var(--fg-faint);
    font-size: 12.5px;
    margin: 0;
  }
</style>
