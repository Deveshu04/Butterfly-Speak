<script lang="ts">
  import { settings } from "$lib/stores.svelte";
  import { undoLearnedCorrection, type Replacement } from "$lib/api";
  import Banner from "$lib/components/Banner.svelte";
  import EmptyState from "$lib/components/EmptyState.svelte";
  import Icon from "$lib/components/Icon.svelte";

  const MAX_WORDS = 100;

  let draft = $state("");
  let corrFrom = $state("");
  let corrTo = $state("");

  let s = $derived(settings.current);
  let words = $derived(s?.dictionary ?? []);
  let atCap = $derived(words.length >= MAX_WORDS);
  let rules = $derived(s?.replacements ?? []);

  /** Why the last word or correction could not be saved, if it could not. */
  let wordError = $state("");
  let ruleError = $state("");

  async function addWord() {
    const word = draft.trim();
    if (!word || !s) return;
    if (words.length >= MAX_WORDS) return;
    const lower = word.toLowerCase();
    if (words.some((w) => w.toLowerCase() === lower)) {
      draft = "";
      return;
    }
    wordError = "";
    try {
      await settings.update((st) => {
        const has = st.dictionary.some((w) => w.toLowerCase() === lower);
        if (!has && st.dictionary.length < MAX_WORDS) {
          st.dictionary = [...st.dictionary, word];
        }
      });
      // Cleared only once the word is saved, so a failed save leaves it in
      // the box to try again. Left alone if something else was typed since.
      if (draft.trim() === word) draft = "";
    } catch (e) {
      console.error("dictionary word not saved:", e);
      wordError = `Couldn't save “${word}”. Try again in a moment.`;
    }
  }

  function removeWord(word: string) {
    settings.update((st) => {
      st.dictionary = st.dictionary.filter((w) => w !== word);
    });
  }

  function onKeydown(e: KeyboardEvent) {
    if (e.key === "Enter") {
      e.preventDefault();
      addWord();
    }
  }

  async function addRule() {
    const from = corrFrom.trim();
    const to = corrTo.trim();
    if (!from || !to || !s) return;
    const lower = from.toLowerCase();
    if (rules.some((r) => r.from.toLowerCase() === lower)) {
      corrFrom = "";
      corrTo = "";
      return;
    }
    ruleError = "";
    try {
      await settings.update((st) => {
        const has = st.replacements.some((r) => r.from.toLowerCase() === lower);
        if (!has) {
          st.replacements = [...st.replacements, { from, to, auto: false }];
        }
      });
      // As with a word: the boxes empty only once the correction is saved.
      if (corrFrom.trim() === from && corrTo.trim() === to) {
        corrFrom = "";
        corrTo = "";
      }
    } catch (e) {
      console.error("correction not saved:", e);
      ruleError = "Couldn't save that correction. Try again in a moment.";
    }
  }

  /** Removing a learned rule is not the same operation as removing one you
   * typed. A promotion wrote two things — the rule, and the evidence that
   * earned it — so dropping only the rule would let the next single
   * correction put it straight back. */
  async function removeRule(rule: Replacement) {
    if (!rule.auto) {
      settings.update((st) => {
        st.replacements = st.replacements.filter((r) => r.from !== rule.from);
      });
      return;
    }
    try {
      await undoLearnedCorrection(rule.from, rule.to);
      // The undo wrote settings on the Rust side, so this window's copy is
      // still the pre-undo snapshot until it is reloaded — and the next
      // settings.update() would clone that stale snapshot right back over
      // it. Same trap as the import path in SystemSection, and `reload`
      // rather than `load` for the same reason: it queues behind a save
      // still in flight. On failure there is nothing to reload: the undo is
      // both-or-neither, so the copy we already hold is still the truth.
      await settings.reload();
    } catch (e) {
      console.error("undo learned correction failed:", e);
    }
  }

  function onRuleKeydown(e: KeyboardEvent) {
    if (e.key === "Enter") {
      e.preventDefault();
      addRule();
    }
  }
</script>

{#if s}
  <div class="page">
    <h1 class="page-title">Dictionary</h1>
    <p class="page-desc">
      Words and corrections that make transcription more accurate.
    </p>

    <Banner
      motif="rings"
      body="Add personal terms, company names, or jargon. They are sent to the recognizer as hints and to AI Polish as preferred spellings."
    >
      Butterfly Speak spells the way <em>you</em> do.
    </Banner>

    <section>
      <div class="section-head">
        <h2 class="section-label">Your words</h2>
        {#if words.length > 0}
          <span class="count">{words.length} / {MAX_WORDS}</span>
        {/if}
      </div>

      <div class="add-row">
        <input
          type="text"
          placeholder="Add a word or name…"
          aria-label="Add a word or name"
          bind:value={draft}
          onkeydown={onKeydown}
          disabled={atCap}
        />
        <button onclick={addWord} disabled={atCap || !draft.trim()}>Add</button>
      </div>
      {#if atCap}
        <p class="note">
          Dictionary is full ({MAX_WORDS} words). Remove a word to add another.
        </p>
      {/if}
      {#if wordError}
        <p class="note" role="alert">{wordError}</p>
      {/if}

      {#if words.length === 0}
        <EmptyState
          icon="book"
          title="No custom words yet"
          body="Add names, products, or jargon Butterfly Speak should recognize."
        />
      {:else}
        <div class="chips" role="list">
          {#each words as word (word)}
            <span class="chip" role="listitem">
              <span class="chip-word">{word}</span>
              <button
                class="chip-remove"
                aria-label="Remove {word}"
                title="Remove"
                onclick={() => removeWord(word)}
              >
                ×
              </button>
            </span>
          {/each}
        </div>
      {/if}
    </section>

    <section>
      <p class="section-label">Corrections</p>
      <p class="intro">
        When a word keeps coming out wrong, map it to the right one. Corrections
        apply instantly after every dictation.
      </p>

      <div class="add-row rule-add">
        <input
          type="text"
          placeholder="Butterfly keeps writing…"
          aria-label="Word that keeps coming out wrong"
          bind:value={corrFrom}
          onkeydown={onRuleKeydown}
        />
        <input
          type="text"
          placeholder="It should be…"
          aria-label="What it should be"
          bind:value={corrTo}
          onkeydown={onRuleKeydown}
        />
        <button onclick={addRule} disabled={!corrFrom.trim() || !corrTo.trim()}>
          Add
        </button>
      </div>
      {#if ruleError}
        <p class="note" role="alert">{ruleError}</p>
      {/if}

      {#if rules.length === 0}
        <EmptyState
          icon="pencil"
          title="No corrections yet"
          body='When a word keeps coming out wrong, map it to the right one — like "draught" → "draft".'
        />
      {:else}
        <div class="rules" role="list">
          {#each rules as rule (rule.from)}
            <div class="rule" role="listitem">
              <span class="chip"><span class="chip-word">{rule.from}</span></span>
              <span class="arrow" aria-hidden="true">→</span>
              <span class="chip"><span class="chip-word">{rule.to}</span></span>
              <span class="spacer"></span>
              {#if rule.auto}
                <span class="learned">Learned</span>
              {/if}
              <button
                class="icon-btn rule-remove"
                aria-label={rule.auto
                  ? `Undo the learned correction ${rule.from} to ${rule.to}`
                  : `Remove correction for ${rule.from}`}
                title={rule.auto ? "Undo" : "Remove"}
                onclick={() => removeRule(rule)}
              >
                <Icon name="trash" size={14} />
              </button>
            </div>
          {/each}
        </div>
      {/if}
    </section>
  </div>
{/if}

<style>
  section {
    margin-bottom: 28px;
  }

  .section-head {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: 12px;
  }

  .count {
    font-size: 12px;
    color: var(--fg-faint);
    font-variant-numeric: tabular-nums;
  }

  .intro {
    color: var(--fg-muted);
    font-size: 13px;
    line-height: 1.6;
    margin: 0 0 14px;
    max-width: 60ch;
  }

  .add-row {
    display: flex;
    gap: 10px;
    margin-top: 4px;
  }

  input[type="text"] {
    flex: 1;
    max-width: 360px;
    font-family: var(--font-ui);
    font-size: 14px;
    padding: 8px 12px;
    border: 1px solid var(--hairline-strong);
    border-radius: var(--radius-control);
    background: var(--surface);
    color: var(--fg);
    transition: border-color var(--motion);
  }

  input[type="text"]::placeholder {
    color: var(--fg-faint);
  }

  input[type="text"]:focus {
    outline: none;
    border-color: var(--fg-faint);
  }

  input[type="text"]:disabled {
    opacity: 0.55;
    cursor: not-allowed;
  }

  .rule-add input[type="text"] {
    max-width: 240px;
  }

  .add-row button {
    font-family: var(--font-ui);
    font-size: 13px;
    font-weight: 600;
    border: 1px solid var(--accent);
    background: var(--accent);
    color: var(--accent-fg);
    border-radius: var(--radius-control);
    padding: 8px 16px;
    cursor: pointer;
    transition: opacity var(--motion);
  }

  .add-row button:disabled {
    opacity: 0.45;
    cursor: default;
  }

  .note {
    color: var(--fg-faint);
    font-size: 13px;
    margin: 8px 2px 0;
    line-height: 1.5;
  }

  .chips {
    display: flex;
    flex-wrap: wrap;
    gap: 10px;
    margin-top: 18px;
  }

  .chip {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    background: var(--surface);
    border: 1px solid var(--hairline-strong);
    border-radius: 999px;
    padding: 7px 14px;
    font-size: 13.5px;
    color: var(--fg);
    line-height: 1.3;
    transition: background var(--motion), border-color var(--motion);
  }

  .chip:hover {
    background: var(--wash);
  }

  .chip-remove {
    appearance: none;
    border: none;
    background: transparent;
    color: var(--fg-faint);
    font-family: var(--font-ui);
    font-size: 14px;
    line-height: 1;
    padding: 0 2px;
    margin-right: -6px;
    cursor: pointer;
    border-radius: 50%;
    opacity: 0;
    transition: opacity var(--motion), color var(--motion);
  }

  .chip:hover .chip-remove,
  .chip-remove:focus-visible {
    opacity: 1;
  }

  .chip-remove:hover {
    color: var(--fg);
  }

  .rules {
    display: flex;
    flex-direction: column;
    margin-top: 14px;
  }

  .rule {
    display: grid;
    grid-template-columns: auto auto auto 1fr auto auto;
    align-items: center;
    gap: 10px;
    padding: 10px 4px;
    border-top: 1px solid var(--hairline);
  }

  .rule .rule-remove {
    grid-column: 6;
  }

  .rule:first-child {
    border-top: none;
  }

  .rule .chip {
    cursor: default;
  }

  .spacer {
    min-width: 0;
  }

  .arrow {
    color: var(--fg-faint);
    font-size: 13px;
  }

  .learned {
    display: inline-flex;
    align-items: center;
    background: var(--teal-soft);
    color: var(--teal);
    font-size: 11px;
    font-weight: 600;
    letter-spacing: 0.02em;
    border-radius: 999px;
    padding: 3px 9px;
    line-height: 1.3;
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
    transition: opacity var(--motion), color var(--motion),
      background var(--motion);
  }

  .icon-btn:hover {
    background: var(--wash);
    color: var(--fg);
  }

  .rule-remove {
    opacity: 0;
  }

  .rule:hover .rule-remove,
  .rule-remove:focus-visible {
    opacity: 1;
  }
</style>
