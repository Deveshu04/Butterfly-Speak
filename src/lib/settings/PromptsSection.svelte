<script lang="ts">
  import { onMount } from "svelte";
  import {
    previewPrompt,
    promptDefaults,
    testPrompt,
    type EditablePrompt,
    type PromptDefault,
    type PromptTestResult,
  } from "$lib/api";
  import { settings } from "$lib/stores.svelte";
  import Dropdown from "$lib/components/Dropdown.svelte";
  import "./rows.css";

  let s = $derived(settings.current);

  /** Copy is deliberately specific about what each kind actually controls —
   * a user editing the Balanced prompt is editing the prompt their next
   * dictation sends, and the label should say so. */
  const EDITABLE_PROMPTS: Array<{ value: EditablePrompt; label: string; sublabel: string }> = [
    {
      value: "light",
      label: "Cleanup — Light",
      sublabel: "Used when Polish level is Light",
    },
    {
      value: "balanced",
      label: "Cleanup — Balanced",
      sublabel: "Used when Polish level is Balanced",
    },
    {
      value: "high",
      label: "Cleanup — High",
      sublabel: "Used when Polish level is High",
    },
    {
      value: "agent",
      label: "Voice agent",
      sublabel: "Used when you address the agent by name",
    },
    {
      value: "selectionRules",
      label: "Agent — selected text",
      sublabel: "Added when you speak with text selected",
    },
  ];

  let kind = $state<EditablePrompt>("balanced");
  /** A cleanup level's prompt, as opposed to one of the agent's. A cleanup
   * reply the app rejects is replaced by its own rule-cleaned text; an agent
   * reply it rejects is replaced by nothing. */
  let cleanupKind = $derived(kind === "light" || kind === "balanced" || kind === "high");
  let defaults = $state<Record<string, string>>({});
  /** The unsaved editor buffer. `null` = not loaded for this kind yet. */
  let draft = $state<string | null>(null);
  let preview = $state("");
  let previewOpen = $state(false);

  let testInput = $state("so um i wanted to say that the meeting is on monday no wait tuesday");
  let testSelection = $state("");
  let running = $state(false);
  let result = $state<PromptTestResult | null>(null);
  let testError = $state<string | null>(null);
  let saveError = $state<string | null>(null);

  let saved = $derived(s ? (s.prompts?.[kind] ?? null) : null);
  let shipped = $derived(defaults[kind] ?? "");
  /** What the editor should show when nothing has been typed yet. */
  let baseline = $derived(saved ?? shipped);
  let text = $derived(draft ?? baseline);
  let dirty = $derived(draft !== null && draft !== baseline);
  /** The same comparison the backend makes authoritatively on save
   * (`PromptOverrides::normalize`: blank, or equal to the shipped text once
   * both are trimmed) — mirrored here only so the button can say what will
   * happen, never as the guard itself. */
  let storesNothing = $derived(text.trim() === "" || text.trim() === shipped.trim());
  let modified = $derived(saved !== null);

  function switchPrompt(v: string) {
    if (!EDITABLE_PROMPTS.some((p) => p.value === v)) return;
    kind = v as EditablePrompt;
    // A draft belongs to the kind it was typed for. Switching kinds discards
    // it rather than carrying it across — silently pasting the agent brief
    // over the Balanced cleanup rules is not an edit anyone asked for.
    draft = null;
    result = null;
    testError = null;
    saveError = null;
    refreshPreview();
  }

  onMount(async () => {
    const list: PromptDefault[] = await promptDefaults();
    defaults = Object.fromEntries(list.map((d) => [d.kind, d.defaultRules]));
    refreshPreview();
  });

  /** Debounced: the preview is a round trip per keystroke otherwise, and the
   * composition it renders is the backend's own — not a copy of it here that
   * could drift. */
  let previewTimer: ReturnType<typeof setTimeout> | undefined;
  function refreshPreview() {
    clearTimeout(previewTimer);
    const forKind = kind;
    const rules = draft;
    previewTimer = setTimeout(async () => {
      try {
        const p = await previewPrompt(forKind, rules);
        // Ignore a reply for a kind the user has since navigated away from.
        if (forKind === kind) preview = p;
      } catch (e) {
        console.error("prompt preview failed:", e);
      }
    }, 200);
  }

  function edit(value: string) {
    draft = value;
    refreshPreview();
  }

  async function save() {
    saveError = null;
    const next = text;
    try {
      await settings.update((st) => {
        // `null`, not `""`: no stored text means the app sends whichever
        // prompt this build ships, so an untouched kind follows app updates.
        // The backend stores null for an unedited default whatever we send.
        st.prompts[kind] = storesNothing ? null : next;
      });
      draft = null;
    } catch (e) {
      saveError = String(e);
    }
  }

  /** Back to the prompt the app ships, stored as "no override": the kind
   * then follows whatever prompt each new version of the app ships. */
  async function resetToDefault() {
    saveError = null;
    try {
      await settings.update((st) => (st.prompts[kind] = null));
      draft = null;
      refreshPreview();
    } catch (e) {
      saveError = String(e);
    }
  }

  /** Run is disabled while a test is running, so a press is one request to
   * the model and presses cannot pile up. Nothing else limits how often it
   * can be pressed once a test has finished. */
  async function run() {
    running = true;
    testError = null;
    result = null;
    // The reply is described by the kind it ran with. A user who switched
    // kinds while it ran is looking at another prompt, so it is dropped.
    const forKind = kind;
    try {
      const reply = await testPrompt(
        kind,
        draft,
        testInput,
        kind === "selectionRules" ? testSelection : null,
      );
      if (forKind === kind) result = reply;
    } catch (e) {
      if (forKind === kind) testError = String(e);
    } finally {
      running = false;
    }
  }
</script>

{#if s}
  <h1 class="s-title">Prompts</h1>

  <p class="s-group">What you are editing</p>
  <div class="s-panel">
    <div class="s-row">
      <div class="s-info">
        <p class="s-row-title">Prompt</p>
        <p class="s-row-sub">
          Each of these is the instruction sent to the AI model for one kind of work. Editing one
          changes what your next dictation actually sends.
        </p>
      </div>
      <Dropdown options={EDITABLE_PROMPTS} value={kind} onchange={switchPrompt} ariaLabel="Prompt to edit" />
    </div>

    <div class="s-row col">
      <div class="s-info">
        <p class="s-row-title">
          Rules
          {#if modified}<span class="pill">Modified</span>{/if}
        </p>
        <p class="s-row-sub">
          {#if kind === "agent"}
            Write <code>&#123;&#123;name&#125;&#125;</code> wherever the agent's name belongs; the app
            puts “{s.agent.name}” in its place before sending.
          {:else if kind === "selectionRules"}
            These rules are added only when you speak a command with text selected. The app always
            follows them with a paragraph of its own that tells the model the selection is text to
            edit, never orders to obey. Nothing you write here can remove it.
          {:else}
            Formatting instructions for the model. Removing a rule removes the behaviour.
          {/if}
        </p>
      </div>
    </div>
    <div class="editor-row">
      <textarea
        class="editor"
        rows="14"
        spellcheck="false"
        aria-label="Prompt rules"
        value={text}
        oninput={(e) => edit(e.currentTarget.value)}
      ></textarea>
      <div class="actions">
        <button class="s-btn primary" onclick={save} disabled={!dirty}>Save</button>
        <button class="s-btn" onclick={resetToDefault} disabled={!modified && storesNothing}>
          Reset to default
        </button>
        {#if dirty}
          <button class="s-btn" onclick={() => { draft = null; refreshPreview(); }}>
            Discard changes
          </button>
        {/if}
        {#if storesNothing && dirty}
          <span class="s-value">
            Blank or the same as the built-in prompt, so nothing is stored: this kind keeps the
            built-in prompt, app updates included.
          </span>
        {/if}
      </div>
      {#if saveError}<p class="s-error">{saveError}</p>{/if}
    </div>

    <div class="s-row col">
      <div class="s-info">
        <p class="s-row-title">What you cannot edit</p>
        <p class="s-row-sub">
          {#if kind === "agent" || kind === "selectionRules"}
            The app appends the reply-language rule, your personal dictionary, the output rules
            and a per-request end marker to whatever you write. Those keep the reply
            free of preamble and let the app notice a reply that was cut off before it is typed
            into your document.
          {:else}
            The app appends the transcript-delimiter rule, your personal dictionary and an
            injection-hardening block to whatever you write, in that order and always after your
            rules. The hardening block is what keeps a dictated "ignore your instructions" written
            down instead of obeyed, and its worked example is tuned per level — so it is not
            editable here.
          {/if}
          Open the full prompt below to see exactly what gets sent.
        </p>
      </div>
    </div>
  </div>

  <p class="s-group">The full prompt</p>
  <div class="s-panel">
    <div class="s-row">
      <div class="s-info">
        <p class="s-row-title">Exactly what is sent</p>
        <p class="s-row-sub">
          Composed by the app, not re-typed here — your rules plus everything appended around
          them.{#if kind === "agent" || kind === "selectionRules"}
            The end marker changes on every request; the one shown is an example.
          {/if}
        </p>
      </div>
      <button class="s-btn" onclick={() => (previewOpen = !previewOpen)}>
        {previewOpen ? "Hide" : "Show"}
      </button>
    </div>
    {#if previewOpen}
      <div class="s-row col">
        <pre class="prompt">{preview}</pre>
      </div>
    {/if}
  </div>

  <p class="s-group">Try it</p>
  <div class="s-panel">
    <div class="s-row col">
      <div class="s-info">
        <p class="s-row-title">
          {kind === "agent" || kind === "selectionRules" ? "Spoken command" : "Dictation"}
        </p>
        <p class="s-row-sub">
          Runs your unsaved draft against the real model, through the same code a dictation uses.
          Nothing is saved, and a dictation running in another window is unaffected.
        </p>
      </div>
      <textarea
        class="editor short"
        rows="3"
        aria-label="Test input"
        bind:value={testInput}
      ></textarea>
      {#if kind === "selectionRules"}
        <div class="s-info">
          <p class="s-row-title">Selected text</p>
          <p class="s-row-sub">Stands in for the text you would have selected when speaking.</p>
        </div>
        <textarea
          class="editor short"
          rows="3"
          aria-label="Selected text"
          bind:value={testSelection}
        ></textarea>
      {/if}
      <div class="actions">
        <button class="s-btn primary" onclick={run} disabled={running}>
          {running ? "Running…" : "Run"}
        </button>
      </div>
      {#if testError}<p class="s-error">{testError}</p>{/if}
    </div>

    {#if result}
      <div class="s-row col">
        <div class="s-info">
          <p class="s-row-title">The model's reply</p>
          <p class="s-row-sub">Before the app checked it.</p>
        </div>
        <pre class="prompt out">{result.rawOutput || "(empty)"}</pre>
      </div>
      <div class="s-row col">
        <div class="s-info">
          <p class="s-row-title">
            What would be typed
            {#if !result.usedModelOutput}<span class="pill warn">Reply discarded</span>{/if}
          </p>
          <p class="s-row-sub">
            {#if result.truncated && cleanupKind}
              The reply was cut off before its end marker, so the app would type its own
              rule-cleaned text instead.
            {:else if result.truncated}
              The reply was cut off before its end marker, so nothing would reach your
              document.
            {:else if !result.usedModelOutput}
              A safety check rejected the model's reply, so the app fell back to its own
              rule-cleaned text.
            {:else}
              <!-- This "safety checks passed" wording assumes
                   `format::guard::REPORT_ONLY` is false. If it is ever set to
                   true, this copy needs a footnote for the agent path. -->
              The safety checks passed, so the model's reply is what you would get.
            {/if}
          </p>
        </div>
        <pre class="prompt out">{result.finalOutput || "(nothing)"}</pre>
        {#if result.notice}
          <p class="s-note">Pill would say: “{result.notice}”</p>
        {/if}
      </div>
    {/if}
  </div>
{/if}

<style>
  .s-row.col {
    flex-direction: column;
    align-items: stretch;
    gap: 10px;
  }

  .editor-row {
    padding: 0 0 22px;
  }

  .editor {
    width: 100%;
    box-sizing: border-box;
    font-family: var(--font-mono, ui-monospace, "Cascadia Mono", Consolas, monospace);
    font-size: 12.5px;
    line-height: 1.6;
    padding: 14px 16px;
    border: 1px solid var(--hairline-strong);
    border-radius: 10px;
    background: var(--surface);
    color: var(--fg);
    resize: vertical;
  }

  .editor.short {
    font-family: var(--font-ui);
    font-size: 13.5px;
  }

  .actions {
    display: flex;
    align-items: center;
    gap: 10px;
    flex-wrap: wrap;
    margin-top: 12px;
  }

  .pill {
    display: inline-block;
    margin-left: 8px;
    font-size: 10.5px;
    font-weight: 700;
    letter-spacing: 0.06em;
    text-transform: uppercase;
    padding: 2px 7px;
    border-radius: 999px;
    background: var(--chip);
    color: var(--fg-muted);
    vertical-align: middle;
  }

  .pill.warn {
    background: var(--danger);
    color: var(--accent-fg);
  }

  .prompt {
    margin: 0;
    max-height: 340px;
    overflow: auto;
    white-space: pre-wrap;
    word-break: break-word;
    font-family: var(--font-mono, ui-monospace, "Cascadia Mono", Consolas, monospace);
    font-size: 12px;
    line-height: 1.6;
    padding: 14px 16px;
    border: 1px solid var(--hairline);
    border-radius: 10px;
    background: var(--surface);
    color: var(--fg-muted);
  }

  .prompt.out {
    color: var(--fg);
    max-height: 220px;
  }

  code {
    font-family: var(--font-mono, ui-monospace, "Cascadia Mono", Consolas, monospace);
    font-size: 12px;
  }
</style>
