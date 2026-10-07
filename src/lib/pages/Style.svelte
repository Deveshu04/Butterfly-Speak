<script lang="ts">
  import { settings } from "$lib/stores.svelte";
  import Dropdown, { type DropdownOption } from "$lib/components/Dropdown.svelte";
  import Icon from "$lib/components/Icon.svelte";
  import type { StylePreset } from "$lib/api";

  interface ToneOption {
    id: StylePreset;
    title: string;
    subtitle: string;
    sample: string;
  }

  const tones: ToneOption[] = [
    {
      id: "formal",
      title: "Formal.",
      subtitle: "Caps + punctuation",
      sample:
        "Hey, are you free for lunch tomorrow? Let's do 12 if that works for you.",
    },
    {
      id: "casual",
      title: "Casual",
      subtitle: "Caps + less punctuation",
      sample:
        "Hey are you free for lunch tomorrow? Let's do 12 if that works for you",
    },
    {
      id: "veryCasual",
      title: "very casual",
      subtitle: "No caps + less punctuation",
      sample:
        "hey are you free for lunch tomorrow? let's do 12 if that works for you",
    },
  ];

  let s = $derived(settings.current);
  let current = $derived(settings.current?.style ?? "formal");

  function choose(id: StylePreset) {
    settings.update((st) => (st.style = id));
  }

  const toneLabels: { id: StylePreset; label: string }[] = [
    { id: "formal", label: "Formal" },
    { id: "casual", label: "Casual" },
    { id: "veryCasual", label: "very casual" },
  ];

  const toneOptions: DropdownOption[] = toneLabels.map((t) => ({
    value: t.id,
    label: t.label,
  }));

  let rules = $derived(settings.current?.styleRules ?? []);

  let newApp = $state("");
  let newTone = $state<StylePreset>("casual");

  function setRuleStyle(index: number, style: string) {
    settings.update((st) => {
      st.styleRules = st.styleRules.map((r, i) =>
        i === index ? { ...r, style } : r,
      );
    });
  }

  function removeRule(index: number) {
    settings.update((st) => {
      st.styleRules = st.styleRules.filter((_, i) => i !== index);
    });
  }

  function addRule() {
    const app = newApp.trim().toLowerCase();
    if (!app) return;
    settings.update((st) => {
      if (st.styleRules.some((r) => r.app === app)) return;
      st.styleRules = [...st.styleRules, { app, style: newTone }];
    });
    newApp = "";
    newTone = "casual";
  }
</script>

{#if s}
  <div class="page">
    <h1 class="page-title">Style</h1>
    <p class="page-desc">How your dictations are written, wherever they land.</p>

    <div class="tones">
      {#each tones as tone (tone.id)}
        <button
          type="button"
          class="card"
          class:selected={current === tone.id}
          aria-pressed={current === tone.id}
          onclick={() => choose(tone.id)}
        >
          <span class="title">{tone.title}</span>
          <span class="subtitle">{tone.subtitle}</span>
          <span class="bubble">{tone.sample}</span>
        </button>
      {/each}
    </div>

    <p class="section-label">App-specific styles</p>
    <p class="section-intro">
      Override the tone for specific apps. Matched against the app's process
      name — the paste target decides.
    </p>

    <div class="rules-card">
      {#if rules.length > 0}
        <ul class="rules-list">
          {#each rules as rule, i (rule.app)}
            <li class="rule-row">
              <span class="app-name">{rule.app}</span>
              <Dropdown
                options={toneOptions}
                value={rule.style}
                onchange={(v) => setRuleStyle(i, v)}
                ariaLabel={`Tone for ${rule.app}`}
              />
              <button
                type="button"
                class="icon-btn"
                onclick={() => removeRule(i)}
                aria-label={`Remove rule for ${rule.app}`}
              >
                <Icon name="trash" size={15} stroke={1.6} />
              </button>
            </li>
          {/each}
        </ul>
      {/if}

      <form
        class="add-row"
        onsubmit={(e) => {
          e.preventDefault();
          addRule();
        }}
      >
        <input
          type="text"
          class="add-input"
          placeholder="Add an app, e.g. whatsapp"
          bind:value={newApp}
        />
        <Dropdown
          options={toneOptions}
          value={newTone}
          onchange={(v) => {
            newTone = v as StylePreset;
          }}
          ariaLabel="Tone for new rule"
        />
        <button type="submit" class="primary-btn" disabled={!newApp.trim()}>
          Add
        </button>
      </form>
    </div>

    <p class="rules-note">
      Defaults: WhatsApp, Telegram, Discord, Signal and Instagram use Casual.
    </p>
  </div>
{/if}

<style>
  .tones {
    display: flex;
    gap: 14px;
    align-items: stretch;
  }

  .card {
    flex: 1 1 0;
    min-width: 0;
    display: flex;
    flex-direction: column;
    align-items: stretch;
    text-align: left;
    background: var(--surface);
    border: 1.5px solid var(--hairline);
    border-radius: var(--radius-card);
    padding: 18px;
    margin: 0;
    font-family: var(--font-ui);
    color: var(--fg);
    cursor: pointer;
    appearance: none;
    transition:
      border-color var(--motion),
      box-shadow var(--motion),
      background var(--motion);
  }

  .card:hover {
    background: var(--bg-elevated);
  }

  .card:focus-visible {
    outline: 2px solid var(--teal);
    outline-offset: 2px;
  }

  .card.selected,
  .card.selected:hover {
    border-color: var(--teal);
    background: var(--surface);
    box-shadow: var(--shadow-selected);
  }

  .title {
    font-size: 15px;
    font-weight: 600;
    line-height: 1.3;
  }

  .subtitle {
    font-size: 13px;
    color: var(--fg-muted);
    margin-top: 3px;
    line-height: 1.4;
  }

  .bubble {
    background: var(--sunken);
    border-radius: 12px;
    padding: 14px;
    font-size: 13.5px;
    line-height: 1.55;
    margin-top: 16px;
    color: var(--fg);
  }

  .section-label {
    margin-top: 28px;
  }

  .section-intro {
    color: var(--fg-muted);
    font-size: 13px;
    margin: 0 2px 14px;
    line-height: 1.5;
  }

  .rules-card {
    background: var(--surface);
    border: 1px solid var(--hairline);
    border-radius: var(--radius-card);
    overflow: hidden;
  }

  .rules-list {
    list-style: none;
    margin: 0;
    padding: 0;
  }

  .rule-row {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 10px 14px;
  }

  .rule-row + .rule-row {
    border-top: 1px solid var(--hairline);
  }

  .app-name {
    font-size: 13.5px;
    font-weight: 550;
    color: var(--fg);
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    /* Pushes the Dropdown + remove button to the row's right edge. */
    margin-right: auto;
  }

  .icon-btn {
    width: 28px;
    height: 28px;
    display: grid;
    place-items: center;
    background: transparent;
    border: none;
    border-radius: 6px;
    color: var(--fg-faint);
    cursor: pointer;
    opacity: 0;
    transition:
      opacity var(--motion),
      background var(--motion),
      color var(--motion);
  }

  .rule-row:hover .icon-btn,
  .icon-btn:focus-visible {
    opacity: 1;
  }

  .icon-btn:hover {
    background: var(--wash);
    color: var(--fg);
  }

  .add-row {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 10px 14px;
  }

  .rules-list + .add-row {
    border-top: 1px solid var(--hairline);
  }

  .add-input {
    flex: 1 1 auto;
    min-width: 0;
    background: transparent;
    border: none;
    padding: 6px 0;
    font-family: var(--font-ui);
    font-size: 13px;
    color: var(--fg);
  }

  .add-input:focus {
    outline: none;
  }

  .add-input::placeholder {
    color: var(--fg-faint);
  }

  .primary-btn {
    background: var(--accent);
    color: var(--accent-fg);
    border: 1px solid var(--accent);
    border-radius: var(--radius-control);
    font-weight: 600;
    font-size: 12.5px;
    padding: 6px 14px;
    font-family: var(--font-ui);
    cursor: pointer;
  }

  .primary-btn:disabled {
    opacity: 0.5;
    cursor: default;
  }

  .rules-note {
    color: var(--fg-faint);
    font-size: 12px;
    margin: 10px 2px 0;
    line-height: 1.5;
  }
</style>
