<script lang="ts">
  // Manage the AI actions a note can be run through: the shipped ones and
  // anything you write yourself.
  //
  // Built-ins are EDITABLE but NOT DELETABLE, and that is enforced twice on
  // purpose: this component doesn't draw the delete control for them, and
  // `notes::actions::delete_action` refuses one anyway. The UI half is what
  // makes the rule obvious; the Rust half is what makes it true. Neither is
  // redundant — dropping the second would leave the rule one devtools call
  // from being broken, and dropping the first would leave a button that
  // exists only to show an error.
  import { onMount } from "svelte";
  import {
    createNoteAction,
    deleteNoteAction,
    listNoteActions,
    updateNoteAction,
    type NoteAction,
  } from "$lib/api";
  import EmptyState from "$lib/components/EmptyState.svelte";
  import Icon from "$lib/components/Icon.svelte";
  import { GLYPHS, NEW_ACTION_GLYPH, glyphFor } from "$lib/notes/glyphs";

  let {
    onchange,
    onclose,
  }: {
    /** The list changed — reload any action menu drawn elsewhere. */
    onchange?: () => void;
    /** Dismiss. Omit it and no Done button is drawn. */
    onclose?: () => void;
  } = $props();

  let actions = $state<NoteAction[]>([]);
  let loading = $state(true);
  let notice = $state("");

  // Editor. editId === null means "new action"; `editing` is what draws it.
  let editing = $state(false);
  let editId = $state<number | null>(null);
  let draftLabel = $state("");
  let draftSummary = $state("");
  let draftInstruction = $state("");
  let draftGlyph = $state(NEW_ACTION_GLYPH);
  let saving = $state(false);

  onMount(reload);

  async function reload() {
    loading = true;
    try {
      actions = await listNoteActions();
    } catch (err) {
      notice = `${err}`;
    } finally {
      loading = false;
    }
  }

  function openEditor(action?: NoteAction) {
    notice = "";
    editId = action?.id ?? null;
    draftLabel = action?.label ?? "";
    draftSummary = action?.summary ?? "";
    draftInstruction = action?.instruction ?? "";
    draftGlyph = action?.glyph ?? NEW_ACTION_GLYPH;
    editing = true;
  }

  function closeEditor() {
    editing = false;
    editId = null;
  }

  async function save() {
    const label = draftLabel.trim();
    const instruction = draftInstruction.trim();
    if (!label || !instruction || saving) return;
    saving = true;
    notice = "";
    try {
      if (editId === null) {
        await createNoteAction({
          label,
          instruction,
          summary: draftSummary.trim(),
          glyph: draftGlyph,
        });
      } else {
        // The editor's four fields; the menu position is left as it is.
        // Editing a built-in goes down this same path — that is the design.
        await updateNoteAction(editId, {
          label,
          instruction,
          summary: draftSummary.trim(),
          glyph: draftGlyph,
        });
      }
      closeEditor();
      await reload();
      onchange?.();
    } catch (err) {
      notice = `${err}`;
    } finally {
      saving = false;
    }
  }

  async function remove(action: NoteAction) {
    // Never reachable for a built-in: the control isn't drawn. The guard is
    // here too so a future caller can't route around the missing button.
    if (action.shipped) return;
    notice = "";
    try {
      await deleteNoteAction(action.id);
      if (editId === action.id) closeEditor();
      await reload();
      onchange?.();
    } catch (err) {
      notice = `${err}`;
    }
  }
</script>

<div class="manager">
  <div class="head">
    <div>
      <p class="title">Note actions</p>
      <p class="sub">
        Each one is a prompt run over the note. The result is saved beside what
        you wrote, never over it.
      </p>
    </div>
    {#if onclose}
      <button class="ghost" onclick={onclose}>Done</button>
    {/if}
  </div>

  {#if notice}
    <p class="notice" role="alert">{notice}</p>
  {/if}

  {#if loading}
    <p class="muted">Loading…</p>
  {:else if actions.length === 0}
    <EmptyState
      icon="sparkles"
      title="No actions yet"
      body="Write one, and it shows up in every note's action menu."
    >
      {#snippet action()}
        <button onclick={() => openEditor()}>Create an action</button>
      {/snippet}
    </EmptyState>
  {:else}
    <ul class="list">
      {#each actions as action (action.id)}
        <li class="row">
          <span class="glyph" aria-hidden="true">
            <Icon name={glyphFor(action.glyph)} size={16} stroke={1.6} />
          </span>
          <span class="text">
            <span class="name">
              {action.label}
              {#if action.shipped}<span class="badge">Built-in</span>{/if}
            </span>
            {#if action.summary}
              <span class="desc">{action.summary}</span>
            {/if}
          </span>
          <span class="row-actions">
            <button
              class="icon-btn"
              title="Edit"
              aria-label="Edit {action.label}"
              onclick={() => openEditor(action)}
            >
              <Icon name="pencil" size={15} stroke={1.6} />
            </button>
            <!--
              Enforcement point two. A built-in gets the badge above instead
              of this button; `notes::actions::delete_action` is point one.
            -->
            {#if !action.shipped}
              <button
                class="icon-btn"
                title="Delete"
                aria-label="Delete {action.label}"
                onclick={() => remove(action)}
              >
                <Icon name="trash" size={15} stroke={1.6} />
              </button>
            {/if}
          </span>
        </li>
      {/each}
    </ul>

    <button class="ghost add" onclick={() => openEditor()}>
      <Icon name="plus" size={15} stroke={1.7} />
      New action
    </button>
  {/if}

  {#if editing}
    <div class="editor">
      <p class="editor-title">
        {editId === null ? "New action" : "Edit action"}
      </p>

      <label class="field">
        <span class="field-label">Name</span>
        <input type="text" bind:value={draftLabel} placeholder="e.g. Bulletise" />
      </label>

      <label class="field">
        <span class="field-label">Description</span>
        <input
          type="text"
          bind:value={draftSummary}
          placeholder="What it's for (optional)"
        />
      </label>

      <label class="field">
        <span class="field-label">Prompt</span>
        <textarea
          rows={4}
          bind:value={draftInstruction}
          placeholder="Turn these notes into short bullets…"
        ></textarea>
      </label>

      <div class="field">
        <span class="field-label">Icon</span>
        <div class="icon-picker">
          {#each GLYPHS as name (name)}
            <button
              class="icon-choice"
              class:selected={draftGlyph === name}
              aria-label={name}
              aria-pressed={draftGlyph === name}
              onclick={() => (draftGlyph = name)}
            >
              <Icon {name} size={16} stroke={1.6} />
            </button>
          {/each}
        </div>
      </div>

      <div class="editor-actions">
        <button class="ghost" onclick={closeEditor}>Cancel</button>
        <button
          class="primary"
          onclick={save}
          disabled={saving || !draftLabel.trim() || !draftInstruction.trim()}
        >
          {saving ? "Saving…" : "Save"}
        </button>
      </div>
    </div>
  {/if}
</div>

<style>
  .manager {
    display: flex;
    flex-direction: column;
    gap: 14px;
  }

  .head {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: 16px;
  }

  .title {
    margin: 0;
    font-size: 15px;
    font-weight: 650;
    color: var(--fg);
  }

  .sub {
    margin: 4px 0 0;
    font-size: 12.5px;
    line-height: 1.5;
    color: var(--fg-muted);
    max-width: 46ch;
  }

  .muted {
    margin: 0;
    font-size: 13px;
    color: var(--fg-faint);
  }

  .notice {
    margin: 0;
    font-size: 12.5px;
    line-height: 1.5;
    color: var(--fg);
    background: var(--wash);
    border: 1px solid var(--hairline-strong);
    border-radius: var(--radius-control);
    padding: 8px 12px;
  }

  .list {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    border: 1px solid var(--hairline);
    border-radius: var(--radius-card);
    overflow: hidden;
  }

  .row {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 10px 12px;
    background: var(--surface);
  }

  .row + .row {
    border-top: 1px solid var(--hairline);
  }

  .glyph {
    flex: none;
    width: 28px;
    height: 28px;
    display: grid;
    place-items: center;
    border-radius: 8px;
    border: 1px solid var(--hairline);
    color: var(--fg-muted);
  }

  .text {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }

  .name {
    font-size: 13.5px;
    font-weight: 600;
    color: var(--fg);
    display: inline-flex;
    align-items: center;
    gap: 8px;
  }

  .badge {
    font-size: 10.5px;
    font-weight: 650;
    letter-spacing: 0.02em;
    text-transform: uppercase;
    color: var(--fg-faint);
    border: 1px solid var(--hairline-strong);
    border-radius: 999px;
    padding: 1px 7px;
  }

  .desc {
    font-size: 12px;
    color: var(--fg-muted);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .row-actions {
    flex: none;
    display: flex;
    gap: 2px;
    opacity: 0;
    transition: opacity 120ms ease;
  }

  .row:hover .row-actions,
  .row:focus-within .row-actions {
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

  .add {
    align-self: flex-start;
    display: inline-flex;
    align-items: center;
    gap: 6px;
  }

  .editor {
    box-sizing: border-box;
    display: flex;
    flex-direction: column;
    gap: 12px;
    background: var(--surface);
    border: 1px solid var(--hairline);
    border-radius: var(--radius-card);
    padding: 16px;
  }

  .editor-title {
    margin: 0;
    font-size: 13px;
    font-weight: 650;
    color: var(--fg-muted);
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

  .icon-picker {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
  }

  .icon-choice {
    width: 32px;
    height: 32px;
    display: grid;
    place-items: center;
    padding: 0;
    background: var(--surface);
    border: 1px solid var(--hairline-strong);
    border-radius: 8px;
    color: var(--fg-muted);
    cursor: pointer;
  }

  .icon-choice.selected {
    border-color: var(--accent);
    color: var(--accent);
  }

  .editor-actions {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
    margin-top: 2px;
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
</style>
