<script lang="ts">
  import "./rows.css";
  import { onMount } from "svelte";
  import Icon from "$lib/components/Icon.svelte";
  import {
    theme,
    prefersReducedMotion,
    onReducedMotionChange,
    type ThemePref,
  } from "$lib/theme.svelte";

  const OPTIONS: Array<{ id: ThemePref; label: string; icon: string }> = [
    { id: "light", label: "Light", icon: "sun" },
    { id: "dark", label: "Dark", icon: "moon" },
    { id: "auto", label: "Auto", icon: "monitor" },
  ];

  let reduceMotion = $state(false);

  onMount(() => {
    reduceMotion = prefersReducedMotion();
    return onReducedMotionChange((v) => (reduceMotion = v));
  });
</script>

<h1 class="s-title">Appearance</h1>

<p class="s-group">Theme</p>
<div class="s-panel">
  <div class="s-row">
    <div class="s-info">
      <p class="s-row-title">Colour theme</p>
      <p class="s-row-sub">
        Auto follows Windows and changes with it. The dictation pill follows
        this too.
      </p>
    </div>
    <div class="seg" role="radiogroup" aria-label="Colour theme">
      {#each OPTIONS as o (o.id)}
        <button
          class="seg-btn"
          class:on={theme.pref === o.id}
          role="radio"
          aria-checked={theme.pref === o.id}
          onclick={() => theme.set(o.id)}
        >
          <Icon name={o.icon} size={15} stroke={1.7} />
          <span>{o.label}</span>
        </button>
      {/each}
    </div>
  </div>
</div>
<p class="s-note">
  The theme is remembered on this PC rather than in your settings file, so it
  can be applied before the window is drawn — that's what keeps the app from
  flashing white on launch. It isn't included in Export settings.
</p>

<p class="s-group">Motion</p>
<div class="s-panel">
  <div class="s-row">
    <div class="s-info">
      <p class="s-row-title">Reduce motion</p>
      <p class="s-row-sub">
        Follows the Windows setting under Accessibility → Visual effects →
        Animation effects. When it's off, Butterfly Speak stops animating: the
        pill's wings and waveform hold still rather than disappearing, so you
        can still see what it's doing.
      </p>
    </div>
    <span class="s-value">{reduceMotion ? "On (from Windows)" : "Off"}</span>
  </div>
</div>

<style>
  /* Segmented control. The row it sits in is already --sunken, so the track
     cannot be: it takes --chip, and the selected segment --chip-raised, which
     is the one pair guaranteed to stay in that order through the theme flip —
     "raised" stops meaning "whiter" in dark and starts meaning "one more step
     of white over whatever is underneath". */
  .seg {
    flex: none;
    display: flex;
    gap: 2px;
    padding: 3px;
    border-radius: 11px;
    background: var(--chip);
  }

  .seg-btn {
    display: flex;
    align-items: center;
    gap: 7px;
    font-family: var(--font-ui);
    font-size: 13px;
    font-weight: 550;
    color: var(--fg-muted);
    background: transparent;
    border: 1px solid transparent;
    border-radius: 8px;
    padding: 7px 13px;
    cursor: pointer;
    transition: background var(--motion), color var(--motion);
  }

  .seg-btn:hover:not(.on) {
    background: var(--wash);
    color: var(--fg);
  }

  .seg-btn.on {
    background: var(--chip-raised);
    border-color: var(--hairline);
    color: var(--fg);
    font-weight: 600;
  }
</style>
