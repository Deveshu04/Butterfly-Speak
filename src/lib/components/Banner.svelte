<script lang="ts">
  import type { Snippet } from "svelte";

  // Distorted-editorial hero card: deep teal field, one dominant warped
  // line-art subject (SVG turbulence displacement gives the organic,
  // hand-inked irregularity), serif headline with an italic accent.
  let {
    motif = "rings",
    body = "",
    children,
    action,
  }: {
    motif?: "rings" | "stack" | "spark" | "waves" | "loop";
    body?: string;
    children?: Snippet;
    action?: Snippet;
  } = $props();

  const uid = Math.random().toString(36).slice(2, 9);
  const warpId = `warp-${uid}`;
  const grainId = `grain-${uid}`;
  const seeds: Record<string, number> = { rings: 7, stack: 11, spark: 3, waves: 5, loop: 9 };
  let seed = $derived(seeds[motif] ?? 7);
</script>

<section class="banner">
  <div class="txt">
    <h2>{@render children?.()}</h2>
    {#if body}
      <p>{body}</p>
    {/if}
    {#if action}
      <div class="action">{@render action()}</div>
    {/if}
  </div>

  <div class="art" aria-hidden="true">
    <svg viewBox="0 0 260 210" fill="none" stroke-linecap="round">
      <defs>
        <filter id={warpId} x="-30%" y="-30%" width="160%" height="160%">
          <feTurbulence type="fractalNoise" baseFrequency="0.012 0.02" numOctaves="2" {seed} result="n" />
          <feDisplacementMap in="SourceGraphic" in2="n" scale="18" xChannelSelector="R" yChannelSelector="G" />
        </filter>
      </defs>
      <g filter="url(#{warpId})" stroke="var(--banner-line)">
        {#if motif === "rings"}
          <circle cx="150" cy="105" r="86" stroke-width="2.5" opacity="0.9" />
          <circle cx="146" cy="102" r="64" stroke-width="2" opacity="0.7" />
          <circle cx="154" cy="108" r="42" stroke-width="2" opacity="0.55" />
          <circle cx="150" cy="105" r="20" stroke-width="2.5" opacity="0.9" fill="#1E5A52" />
          <circle cx="150" cy="105" r="5" fill="#EFE9D8" stroke="none" />
        {:else if motif === "stack"}
          <rect x="70" y="38" width="130" height="26" rx="13" stroke-width="2.5" opacity="0.9" />
          <rect x="88" y="76" width="112" height="26" rx="13" stroke-width="2" opacity="0.7" />
          <rect x="62" y="114" width="146" height="26" rx="13" stroke-width="2" opacity="0.55" />
          <rect x="96" y="152" width="86" height="26" rx="13" stroke-width="2.5" opacity="0.85" fill="#1E5A52" />
          <path d="M110 165h56" stroke-width="2.5" opacity="0.9" />
        {:else if motif === "spark"}
          <path d="M150 22v166" stroke-width="2.5" opacity="0.85" />
          <path d="M67 105h166" stroke-width="2.5" opacity="0.85" />
          <path d="M96 51l108 108" stroke-width="2" opacity="0.6" />
          <path d="M204 51L96 159" stroke-width="2" opacity="0.6" />
          <circle cx="150" cy="105" r="30" stroke-width="2.5" opacity="0.9" fill="#1E5A52" />
          <circle cx="150" cy="105" r="7" fill="#EFE9D8" stroke="none" />
        {:else if motif === "waves"}
          <path d="M40 60c30-24 60 24 90 0s60 24 90 0" stroke-width="2.5" opacity="0.9" />
          <path d="M40 100c30-24 60 24 90 0s60 24 90 0" stroke-width="2" opacity="0.7" />
          <path d="M40 140c30-24 60 24 90 0s60 24 90 0" stroke-width="2" opacity="0.55" />
          <path d="M40 180c30-24 60 24 90 0s60 24 90 0" stroke-width="2.5" opacity="0.4" />
          <circle cx="196" cy="88" r="14" stroke-width="2.5" opacity="0.95" fill="#1E5A52" />
        {:else}
          <path d="M76 150c-24-52 34-108 74-92s58 62 24 90-72 14-64-26 66-52 96-20" stroke-width="2.5" opacity="0.9" />
          <circle cx="76" cy="150" r="8" fill="#EFE9D8" stroke="none" />
          <circle cx="206" cy="102" r="8" stroke-width="2.5" opacity="0.9" fill="#1E5A52" />
        {/if}
      </g>
    </svg>
  </div>

  <svg class="grain" aria-hidden="true">
    <filter id={grainId}>
      <feTurbulence type="fractalNoise" baseFrequency="0.9" numOctaves="2" />
    </filter>
    <rect width="100%" height="100%" filter="url(#{grainId})" />
  </svg>
</section>

<style>
  .banner {
    position: relative;
    display: flex;
    align-items: center;
    gap: 20px;
    border-radius: var(--radius-hero);
    background:
      radial-gradient(120% 170% at 86% 8%, rgba(46, 143, 124, 0.38), transparent 56%),
      linear-gradient(108deg, var(--banner-bg-a) 0%, var(--banner-bg-b) 55%, var(--banner-bg-c) 100%);
    padding: 32px 36px;
    margin-bottom: 30px;
    overflow: hidden;
    min-height: 176px;
  }

  .txt {
    position: relative;
    z-index: 2;
    max-width: 60%;
    min-width: 0;
  }

  h2 {
    font-family: var(--font-display);
    font-size: 26px;
    font-weight: 500;
    letter-spacing: 0.005em;
    line-height: 1.3;
    color: var(--banner-fg);
    margin: 0 0 10px;
  }

  h2 :global(em) {
    font-style: italic;
    font-weight: 500;
  }

  p {
    font-size: 14px;
    line-height: 1.6;
    color: var(--banner-fg-muted);
    margin: 0;
    max-width: 58ch;
  }

  .action {
    margin-top: 16px;
  }

  .action :global(button) {
    font-family: var(--font-ui);
    font-size: 13px;
    font-weight: 600;
    border: none;
    border-radius: var(--radius-control);
    background: var(--banner-fg);
    color: #0b342e;
    padding: 9px 18px;
    cursor: pointer;
  }

  /* The illustration is the graphic identity of the page, not an icon in a
     corner: large, bleeding past the edges. */
  .art {
    position: absolute;
    right: -28px;
    top: 50%;
    transform: translateY(-50%);
    width: 320px;
    height: 258px;
    z-index: 1;
    pointer-events: none;
  }

  .art svg {
    width: 100%;
    height: 100%;
  }

  .grain {
    position: absolute;
    inset: 0;
    width: 100%;
    height: 100%;
    opacity: 0.05;
    mix-blend-mode: overlay;
    pointer-events: none;
    z-index: 3;
  }

  @media (max-width: 900px) {
    .txt {
      max-width: 100%;
    }
    .art {
      opacity: 0.35;
    }
  }
</style>
