<script lang="ts">
  import { onMount } from "svelte";
  import { listen } from "@tauri-apps/api/event";
  import {
    downloadModel,
    formatBytes,
    polishStatus,
    type CleanupLevel,
    type PolishStatus,
  } from "$lib/api";
  import { MODEL_PROGRESS, type ModelProgressPayload } from "$lib/events";
  import { settings } from "$lib/stores.svelte";
  import Dropdown from "$lib/components/Dropdown.svelte";
  import Switch from "./Switch.svelte";
  import "./rows.css";

  let s = $derived(settings.current);
  /** A cloud engine — the user's own Sarvam key or Butterfly Labs' relay.
   * Both polish through the same chat call, so neither needs the on-device
   * polish model or the rows that manage it. */
  let cloud = $derived(s?.provider === "sarvam" || s?.provider === "cloud");
  let polish = $state<PolishStatus | null>(null);
  let polishProgress = $state<ModelProgressPayload | null>(null);
  /** Why the last polish model download failed, until the next one starts. */
  let polishFailure = $state("");

  onMount(() => {
    polishStatus().then((p) => (polish = p));
    let unsub: (() => void) | undefined;
    listen<ModelProgressPayload>(MODEL_PROGRESS, (e) => {
      if (e.payload.id !== "aiPolish") return;
      polishProgress = e.payload;
      if (e.payload.phase === "error") {
        polishFailure = `Download failed: ${e.payload.message ?? "unknown error"}`;
      }
      if (["done", "error", "cancelled"].includes(e.payload.phase)) {
        polishStatus().then((p) => (polish = p));
        polishProgress = null;
      }
    }).then((u) => (unsub = u));
    return () => unsub?.();
  });

  function downloadPolish() {
    polishFailure = "";
    downloadModel("aiPolish").catch(() => {});
  }

  /** `Dropdown`'s `onchange` contract is a plain `string` — narrow for real
   * rather than asserting, so a typo elsewhere can't sail a bad value into
   * settings. */
  function isCleanupLevel(v: string): v is CleanupLevel {
    return v === "off" || v === "light" || v === "balanced" || v === "high";
  }

  /** Moving off "off" with the on-device provider fetches the model weights
   * right then — the explicit user action. Gated on that exact transition
   * (`wasOff`) so switching between two non-off levels — e.g. after a failed
   * download left the model not-installed — never silently re-fires the
   * fetch; the user only asked for a different level, not a retry. */
  async function setLevel(level: string) {
    if (!isCleanupLevel(level)) return;
    const wasOff = s?.cleanup.level === "off";
    await settings.update((st) => (st.cleanup.level = level));
    if (
      wasOff &&
      level !== "off" &&
      !cloud &&
      polish?.available &&
      !polish.installed &&
      !polish.downloading
    ) {
      downloadPolish();
      polish = await polishStatus();
    }
  }

  function pct(p: ModelProgressPayload): number {
    if (p.total === 0) return 0;
    return Math.min(100, Math.round((p.downloaded / p.total) * 100));
  }

  let polishSize = $derived(polish ? formatBytes(polish.diskBytes) : "491 MB");

  /** `value` is typed to `CleanupLevel`, not `string` — a typo here (e.g.
   * "hgih") fails to compile instead of silently reaching settings. */
  const levelOptions: Array<{ value: CleanupLevel; label: string; sublabel: string }> = [
    { value: "off", label: "Off", sublabel: "Rules only — no AI rewriting" },
    {
      value: "light",
      label: "Light",
      sublabel: "Punctuation, casing and numbers — keeps fillers and false starts",
    },
    {
      value: "balanced",
      label: "Balanced",
      sublabel: "Also removes fillers and false starts",
    },
    {
      value: "high",
      label: "High",
      sublabel: "Also applies self-corrections and builds lists",
    },
  ];
</script>

{#if s}
  <h1 class="s-title">Cleanup</h1>

  <p class="s-group">While you speak</p>
  <div class="s-panel">
    <div class="s-row">
      <div class="s-info">
        <p class="s-row-title" id="set-spoken-commands">Spoken commands</p>
        <p class="s-row-sub">"new line" and "new paragraph" insert real line breaks.</p>
      </div>
      <Switch
        labelledby="set-spoken-commands"
        checked={s.cleanup.spokenCommands}
        onchange={(on) => settings.update((st) => (st.cleanup.spokenCommands = on))}
      />
    </div>

    <div class="s-row">
      <div class="s-info">
        <p class="s-row-title" id="set-smart-spacing">Smart spacing</p>
        <p class="s-row-sub">
          Adds a trailing space after dictated text so the next word you type doesn't run into it.
        </p>
      </div>
      <Switch
        labelledby="set-smart-spacing"
        checked={s.dictation.smartSpace}
        onchange={(on) => settings.update((st) => (st.dictation.smartSpace = on))}
      />
    </div>

    {#if !cloud}
      <div class="s-row">
        <div class="s-info">
          <p class="s-row-title" id="set-fillers">Remove filler words</p>
          <p class="s-row-sub">
            Drops "um", "uh", "hmm" and hesitations. Guarded phrases like "as you know" are kept.
          </p>
        </div>
        <Switch
          labelledby="set-fillers"
          checked={s.cleanup.fillers}
          onchange={(on) => settings.update((st) => (st.cleanup.fillers = on))}
        />
      </div>

      <div class="s-row">
        <div class="s-info">
          <p class="s-row-title" id="set-like">Also remove "like"</p>
          <p class="s-row-sub">
            More aggressive: strips conversational "like" too. Off by default because it sometimes
            removes real ones.
          </p>
        </div>
        <Switch
          labelledby="set-like"
          checked={s.cleanup.aggressiveFillers}
          disabled={!s.cleanup.fillers}
          onchange={(on) => settings.update((st) => (st.cleanup.aggressiveFillers = on))}
        />
      </div>

      <div class="s-row">
        <div class="s-info">
          <p class="s-row-title" id="set-self-correction">Self-correction</p>
          <p class="s-row-sub">
            "Meet on Monday — no wait, on Tuesday" types just "Meet on Tuesday". Triggers: no wait,
            no I meant, scratch that, let me rephrase.
          </p>
        </div>
        <Switch
          labelledby="set-self-correction"
          checked={s.cleanup.backtrack}
          onchange={(on) => settings.update((st) => (st.cleanup.backtrack = on))}
        />
      </div>

      <div class="s-row">
        <div class="s-info">
          <p class="s-row-title" id="set-punctuation">Punctuation & capitalization</p>
          <p class="s-row-sub">
            Adds periods, commas and casing. Skipped automatically for models with built-in
            punctuation.
          </p>
        </div>
        <Switch
          labelledby="set-punctuation"
          checked={s.cleanup.punctuation}
          onchange={(on) => settings.update((st) => (st.cleanup.punctuation = on))}
        />
      </div>

      <div class="s-row">
        <div class="s-info">
          <p class="s-row-title" id="set-itn">Format numbers, times & dates</p>
          <p class="s-row-sub">
            "three thirty pm" → 3:30 PM · "twenty five percent" → 25% · "january fifth" → January
            5th
          </p>
        </div>
        <Switch
          labelledby="set-itn"
          checked={s.cleanup.itn}
          onchange={(on) => settings.update((st) => (st.cleanup.itn = on))}
        />
      </div>
    {/if}
  </div>

  <p class="s-group">AI Polish</p>
  <div class="s-panel">
    {#if !cloud && polish && !polish.available}
      <div class="s-row">
        <div class="s-info">
          <p class="s-row-sub">On-device polish isn't included in this build.</p>
        </div>
      </div>
    {:else}
      <div class="s-row">
        <div class="s-info">
          <p class="s-row-title">Polish level</p>
          <p class="s-row-sub">
            {cloud
              ? "How much the AI may edit what you said, from light formatting up to full cleanup with filler and self-correction removal. Off skips the AI call entirely; every other level adds about a second per dictation."
              : `How much the on-device model may edit what you said, from light formatting up to full cleanup with filler and self-correction removal. Downloads a small model (${polishSize}) the first time you set this above Off.`}
          </p>
        </div>
        <Dropdown options={levelOptions} value={s.cleanup.level} onchange={setLevel} />
      </div>

      {#if !cloud && s.cleanup.level !== "off" && polish && !polish.installed}
        <div class="s-row">
          {#if polishProgress && polishProgress.phase === "downloading"}
            <div class="s-info">
              <p class="s-row-title">Downloading the polish model</p>
            </div>
            <div class="dl">
              <div class="progress">
                <div class="bar" style="width: {pct(polishProgress)}%"></div>
              </div>
              <span class="s-value">{pct(polishProgress)}%</span>
            </div>
          {:else if polish.downloading || polishProgress}
            <div class="s-info">
              <p class="s-row-title">Polish model</p>
              <p class="s-row-sub">Preparing the polish model…</p>
            </div>
          {:else}
            <div class="s-info">
              <p class="s-row-title">Polish model</p>
              <p class="s-row-sub">Not installed yet.</p>
              {#if polishFailure}
                <p class="s-error">{polishFailure}</p>
              {/if}
            </div>
            <button class="s-btn" onclick={downloadPolish}>
              Download polish model ({formatBytes(polish.diskBytes)})
            </button>
          {/if}
        </div>
      {/if}
    {/if}
  </div>
{/if}

<style>
  .dl {
    display: flex;
    align-items: center;
    gap: 12px;
    flex: none;
  }

  .progress {
    width: 160px;
    height: 6px;
    border-radius: 999px;
    background: var(--bg-elevated);
    border: 1px solid var(--hairline);
    overflow: hidden;
  }

  .bar {
    height: 100%;
    background: var(--accent);
    transition: width 200ms linear;
  }
</style>
