<script lang="ts">
  import type { Snippet as SnippetEntry } from "$lib/api";
  import { settings } from "$lib/stores.svelte";
  import Banner from "$lib/components/Banner.svelte";
  import EmptyState from "$lib/components/EmptyState.svelte";
  import Icon from "$lib/components/Icon.svelte";

  let s = $derived(settings.current);

  let trigger = $state("");
  let expansion = $state("");
  let formOpen = $state(false);
  let editingIndex = $state<number | null>(null);
  let copiedIndex = $state<number | null>(null);
  let copiedTimer: ReturnType<typeof setTimeout> | undefined;

  let canSave = $derived(trigger.trim().length > 0 && expansion.trim().length > 0);
  /** Why the form did not save, or "". */
  let formError = $state("");
  /** The snippet an Add will replace, once the user has been told; null
   * until then, and again whenever the trigger is edited. */
  let replacing = $state<number | null>(null);

  /** Triggers fire case-insensitively with any run of spaces between their
   * words (`cleanup::snippets`), so two triggers that differ only in those
   * are one trigger, and only the first of them could ever fire. */
  function triggerKey(t: string): string {
    return t.normalize("NFC").trim().toLowerCase().split(/\s+/).join(" ");
  }

  function triggerEdited() {
    replacing = null;
    formError = "";
  }

  function openAdd() {
    editingIndex = null;
    trigger = "";
    expansion = "";
    formOpen = true;
  }

  function startEdit(index: number, snip: SnippetEntry) {
    editingIndex = index;
    trigger = snip.trigger;
    expansion = snip.expansion;
    formOpen = true;
  }

  function cancelForm() {
    formOpen = false;
    editingIndex = null;
    trigger = "";
    expansion = "";
    triggerEdited();
  }

  /**
   * Save the form. An edit may not take another snippet's trigger. An Add
   * with a trigger that is already in use says so first, and replaces that
   * snippet only when saved a second time.
   */
  async function saveSnippet() {
    const t = trigger.trim();
    const e = expansion.trim();
    if (!t || !e || !s) return;
    const editing = editingIndex;
    const key = triggerKey(t);
    const clash = s.snippets.findIndex((sn, i) => i !== editing && triggerKey(sn.trigger) === key);
    if (clash >= 0 && editing !== null) {
      formError = `You already have a snippet for “${s.snippets[clash].trigger}”. Change that one, or pick another trigger.`;
      return;
    }
    if (clash >= 0 && replacing !== clash) {
      replacing = clash;
      return;
    }
    try {
      await settings.update((st) => {
        if (editing !== null && editing >= 0 && editing < st.snippets.length) {
          const next = [...st.snippets];
          next[editing] = { trigger: t, expansion: e };
          st.snippets = next;
        } else {
          const next: SnippetEntry[] = st.snippets.filter(
            (sn) => triggerKey(sn.trigger) !== key,
          );
          next.push({ trigger: t, expansion: e });
          st.snippets = next;
        }
      });
    } catch (err) {
      console.error("snippet save failed:", err);
      formError = "Couldn't save that snippet. Try again in a moment.";
      return;
    }
    cancelForm();
  }

  async function removeSnippet(index: number) {
    if (editingIndex === index) cancelForm();
    else if (editingIndex !== null && editingIndex > index) editingIndex -= 1;
    if (copiedIndex === index) copiedIndex = null;
    await settings.update((st) => {
      st.snippets = st.snippets.filter((_, i) => i !== index);
    });
  }

  async function copyExpansion(index: number, text: string) {
    try {
      await navigator.clipboard.writeText(text);
      copiedIndex = index;
      clearTimeout(copiedTimer);
      copiedTimer = setTimeout(() => (copiedIndex = null), 1400);
    } catch {
      // clipboard unavailable — ignore
    }
  }
</script>

{#snippet snippetForm()}
  <form
    class="form-card"
    onsubmit={(e) => {
      e.preventDefault();
      saveSnippet();
    }}
  >
    <div class="fields">
      <label class="field">
        <span class="field-label">Say this…</span>
        <input
          type="text"
          placeholder="my linkedin"
          bind:value={trigger}
          oninput={triggerEdited}
          spellcheck="false"
          autocomplete="off"
        />
      </label>
      <label class="field grow">
        <span class="field-label">Type this…</span>
        <textarea
          rows="2"
          placeholder="https://linkedin.com/in/you"
          bind:value={expansion}
          spellcheck="false"
        ></textarea>
      </label>
    </div>
    <div class="actions">
      <button type="submit" class="primary" disabled={!canSave}>
        {editingIndex !== null ? "Save" : replacing !== null ? "Replace" : "Add snippet"}
      </button>
      <button type="button" class="ghost" onclick={cancelForm}>Cancel</button>
      <p class="hint">
        Triggers aren’t case-sensitive — saying the phrase mid-dictation drops
        the text right in.
      </p>
    </div>
    {#if formError}
      <p class="form-error" role="alert">{formError}</p>
    {:else if replacing !== null && s?.snippets[replacing]}
      <p class="form-note" role="status">
        You already have a snippet for “{s.snippets[replacing].trigger}”. Replace it with this
        one?
      </p>
    {/if}
  </form>
{/snippet}

{#if s}
  <div class="page">
    <h1 class="page-title">Snippets</h1>
    <p class="page-desc">Reusable text you can drop in by voice.</p>

    <Banner
      motif="stack"
      body="Save text you use often — a link, an intro, a prompt — then say the trigger phrase while dictating to type it instantly."
    >
      The stuff <em>you</em> shouldn’t have to re-type.
    </Banner>

    <section>
      <p class="section-label">Your snippets</p>

      {#if s.snippets.length === 0 && !formOpen}
        <EmptyState
          icon="scissors"
          title="No snippets yet"
          body={'Save text you re-type all the time — try "my email" → your address.'}
        >
          {#snippet action()}
            <button onclick={openAdd}>Add your first snippet</button>
          {/snippet}
        </EmptyState>
      {:else}
        {#if !formOpen}
          <div class="add-row">
            <button class="primary add-btn" onclick={openAdd}>
              <Icon name="plus" size={15} />
              Add snippet
            </button>
          </div>
        {:else if editingIndex === null}
          {@render snippetForm()}
        {/if}

        {#each s.snippets as snip, i (snip.trigger + "|" + i)}
          {#if editingIndex === i && formOpen}
            {@render snippetForm()}
          {:else}
            <div class="snippet-card">
              <div class="snippet-text">
                <p class="trigger">“{snip.trigger}”</p>
                <p class="expansion" title={snip.expansion}>{snip.expansion}</p>
              </div>
              <div class="row-actions">
                <button
                  class="icon-btn"
                  class:copied={copiedIndex === i}
                  aria-label={`Copy expansion of “${snip.trigger}”`}
                  title={copiedIndex === i ? "Copied" : "Copy"}
                  onclick={() => copyExpansion(i, snip.expansion)}
                >
                  {#if copiedIndex === i}
                    <span class="check" aria-hidden="true">✓</span>
                  {:else}
                    <Icon name="copy" size={15} stroke={1.6} />
                  {/if}
                </button>
                <button
                  class="icon-btn"
                  aria-label={`Edit snippet “${snip.trigger}”`}
                  title="Edit"
                  onclick={() => startEdit(i, snip)}
                >
                  <Icon name="pencil" size={15} stroke={1.6} />
                </button>
                <button
                  class="icon-btn"
                  aria-label={`Delete snippet “${snip.trigger}”`}
                  title="Delete"
                  onclick={() => removeSnippet(i)}
                >
                  <Icon name="trash" size={15} stroke={1.6} />
                </button>
              </div>
            </div>
          {/if}
        {/each}
      {/if}
    </section>
  </div>
{/if}

<style>
  section {
    margin-bottom: 28px;
  }

  /* ---- Snippet cards (the library) ---- */
  .snippet-card {
    display: flex;
    align-items: flex-start;
    gap: 12px;
    background: var(--surface);
    border: 1px solid var(--hairline);
    border-radius: var(--radius-card);
    padding: 16px 18px;
    margin-bottom: 10px;
    min-width: 0;
  }

  .snippet-text {
    flex: 1 1 auto;
    min-width: 0;
  }

  .trigger {
    margin: 0 0 3px;
    font-size: 14.5px;
    font-weight: 600;
    color: var(--fg);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .expansion {
    margin: 0;
    font-size: 13px;
    color: var(--fg-muted);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .row-actions {
    flex: 0 0 auto;
    display: flex;
    gap: 2px;
  }

  .icon-btn {
    width: 28px;
    height: 28px;
    display: grid;
    place-items: center;
    background: transparent;
    border: none;
    border-radius: 6px;
    padding: 0;
    color: var(--fg-faint);
    cursor: pointer;
    opacity: 0;
    transition:
      opacity var(--motion),
      background var(--motion),
      color var(--motion);
  }

  .snippet-card:hover .icon-btn,
  .icon-btn:focus-visible,
  .icon-btn.copied {
    opacity: 1;
  }

  .icon-btn:hover {
    background: var(--wash);
    color: var(--fg);
  }

  .icon-btn.copied,
  .icon-btn.copied:hover {
    color: var(--accent);
  }

  .check {
    font-size: 14px;
    font-weight: 700;
    line-height: 1;
  }

  /* ---- Add button row ---- */
  .add-row {
    margin-bottom: 12px;
  }

  .add-btn {
    display: inline-flex;
    align-items: center;
    gap: 6px;
  }

  /* ---- Add / edit form ---- */
  .form-card {
    background: var(--bg-elevated);
    border: 1px solid var(--hairline);
    border-radius: var(--radius-card);
    padding: 20px;
    margin-bottom: 12px;
  }

  .fields {
    display: flex;
    gap: 14px;
    align-items: flex-start;
    flex-wrap: wrap;
  }

  .field {
    display: flex;
    flex-direction: column;
    gap: 6px;
    flex: 1 1 200px;
    min-width: 180px;
  }

  .field.grow {
    flex: 2 1 300px;
  }

  .field-label {
    font-size: 11px;
    font-weight: 600;
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--fg-faint);
  }

  input,
  textarea {
    font-family: var(--font-ui);
    font-size: 13.5px;
    color: var(--fg);
    background: var(--surface);
    border: 1px solid var(--hairline-strong);
    border-radius: var(--radius-control);
    padding: 8px 12px;
    width: 100%;
    box-sizing: border-box;
    transition: border-color var(--motion);
  }

  textarea {
    resize: vertical;
    min-height: 40px;
    line-height: 1.5;
  }

  input:focus,
  textarea:focus {
    outline: none;
    border-color: var(--fg-faint);
  }

  input::placeholder,
  textarea::placeholder {
    color: var(--fg-faint);
  }

  .actions {
    display: flex;
    align-items: center;
    gap: 10px;
    margin-top: 14px;
  }

  .hint {
    margin: 0 0 0 4px;
    color: var(--fg-faint);
    font-size: 12.5px;
    line-height: 1.5;
  }

  .form-error,
  .form-note {
    margin: 12px 0 0;
    font-size: 13px;
    line-height: 1.5;
  }

  .form-error {
    color: var(--danger);
  }

  .form-note {
    color: var(--fg);
  }

  /* ---- Buttons ---- */
  button {
    font-family: var(--font-ui);
    font-size: 13px;
    font-weight: 600;
    border-radius: var(--radius-control);
    padding: 8px 16px;
    cursor: pointer;
    transition:
      background var(--motion),
      opacity var(--motion);
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
    color: var(--fg);
    border: 1px solid var(--hairline);
    padding: 7px 14px;
    font-size: 12.5px;
  }

  button.ghost:hover {
    background: var(--wash);
  }
</style>
