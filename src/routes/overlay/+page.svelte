<script lang="ts">
  import { listen } from "@tauri-apps/api/event";
  import { onMount, untrack } from "svelte";
  import {
    STATE_CHANGED,
    OVERLAY_STATUS,
    LEVEL,
    NOTICE_ERROR,
    type DictationState,
    type LevelPayload,
    type NoticePayload,
    type StatusPayload,
    type StatePayload,
  } from "$lib/events";
  import {
    theme,
    colorToken,
    prefersReducedMotion,
    onReducedMotionChange,
  } from "$lib/theme.svelte";

  const BAR_COUNT = 24;
  const WAVE_FALLBACK = "rgba(255, 255, 255, 0.9)";

  let dictation = $state<DictationState>("idle");
  let handsFree = $state(false);
  // A short status line ("Polish…") while a transform runs. The pill never
  // shows live transcript text: watching a half-decoded phrase rewrite itself
  // makes people second-guess what they just said.
  let status = $state("");
  let errorMsg = $state("");
  let canvas: HTMLCanvasElement | undefined = $state();

  // Waveform state lives outside Svelte reactivity: it's redrawn by rAF.
  let targets = new Array<number>(BAR_COUNT).fill(0);
  let heights = new Array<number>(BAR_COUNT).fill(0);
  let raf = 0;
  // The bar colour is a CSS token, but a canvas can't read one — it has to be
  // resolved to a literal and re-resolved whenever the theme moves under it.
  let waveColor = WAVE_FALLBACK;
  let reduceMotion = $state(false);

  function pushLevel(level: number) {
    // Map RMS (~0..0.15 speech) to 0..1 with a soft knee.
    const v = Math.min(1, Math.pow(level * 9, 0.7));
    targets.shift();
    targets.push(v);
  }

  /** Clear the canvas and hand back what both painters need. */
  function bars(): { ctx: CanvasRenderingContext2D; h: number; bw: number } | null {
    if (!canvas) return null;
    const ctx = canvas.getContext("2d");
    if (!ctx) return null;
    const { width, height } = canvas;
    ctx.clearRect(0, 0, width, height);
    ctx.fillStyle = waveColor;
    return { ctx, h: height, bw: (width - 3 * (BAR_COUNT - 1)) / BAR_COUNT };
  }

  function bar(ctx: CanvasRenderingContext2D, i: number, bw: number, h: number, unit: number) {
    const bh = Math.max(3, unit * (h - 8));
    const x = i * (bw + 3);
    ctx.beginPath();
    ctx.roundRect(x, (h - bh) / 2, bw, bh, bw / 2);
    ctx.fill();
  }

  function draw() {
    raf = requestAnimationFrame(draw);
    const c = bars();
    if (!c) return;
    const frozen = dictation === "finalizing";
    for (let i = 0; i < BAR_COUNT; i++) {
      if (!frozen) {
        heights[i] += (targets[i] - heights[i]) * 0.35;
      }
      bar(c.ctx, i, c.bw, c.h, heights[i]);
    }
  }

  /** The reduced-motion waveform: one frame, held.
   *
   *  Freezing it flat, or hiding it, would take away the pill's only visual
   *  answer to "is it still listening?" — so instead the bars settle into a
   *  symmetric resting envelope that still reads as sound, and the status line
   *  beside them carries the live state. Dimmed while finalizing, the same cue
   *  the animated version gives. */
  function drawResting() {
    const c = bars();
    if (!c) return;
    c.ctx.globalAlpha = dictation === "finalizing" ? 0.55 : 1;
    for (let i = 0; i < BAR_COUNT; i++) {
      const t = i / (BAR_COUNT - 1);
      bar(c.ctx, i, c.bw, c.h, 0.22 + 0.5 * Math.sin(Math.PI * t));
    }
    c.ctx.globalAlpha = 1;
  }

  function applyMotionMode() {
    if (reduceMotion) {
      if (raf) cancelAnimationFrame(raf);
      raf = 0;
      drawResting();
    } else if (!raf) {
      raf = requestAnimationFrame(draw);
    }
  }

  // Repaint the held frame whenever something it cannot notice on its own has
  // moved. A running rAF loop picks all of this up by itself; a frozen canvas
  // has to be told.
  //
  // `canvas` is one of those things: an error notice replaces the canvas with
  // a message, so the element is destroyed and recreated. The STATE_CHANGED
  // handler clears the error and repaints in the same tick, before `bind:this`
  // has re-populated the binding, so that repaint hits a null canvas and the
  // frozen path — having no loop to heal itself — would stay blank for the
  // whole of the next recording. Depending on the element here means the
  // remount itself triggers the repaint, in the tick where it actually exists.
  //
  // The draw is untracked so this stays an effect about *its own* dependencies:
  // drawResting() reads `dictation`, and without untrack that read would
  // quietly become one, duplicating the STATE_CHANGED handler's repaint.
  $effect(() => {
    void theme.resolved;
    void canvas;
    const reduced = reduceMotion;
    waveColor = colorToken("--pill-wave", WAVE_FALLBACK);
    if (reduced) untrack(() => drawResting());
  });

  onMount(() => {
    const unsubs: Array<() => void> = [];
    listen<StatePayload>(STATE_CHANGED, (e) => {
      dictation = e.payload.state;
      handsFree = e.payload.mode === "handsFree";
      if (dictation === "recording") {
        status = "";
        errorMsg = "";
        targets.fill(0);
        heights.fill(0);
      }
      if (reduceMotion) drawResting();
    }).then((u) => unsubs.push(u));
    listen<StatusPayload>(OVERLAY_STATUS, (e) => {
      status = e.payload.text;
    }).then((u) => unsubs.push(u));
    listen<LevelPayload>(LEVEL, (e) => pushLevel(e.payload.level)).then((u) =>
      unsubs.push(u),
    );
    listen<NoticePayload>(NOTICE_ERROR, (e) => {
      errorMsg = e.payload.message;
    }).then((u) => unsubs.push(u));

    reduceMotion = prefersReducedMotion();
    applyMotionMode();
    const offMotion = onReducedMotionChange((v) => {
      reduceMotion = v;
      applyMotionMode();
    });

    return () => {
      if (raf) cancelAnimationFrame(raf);
      raf = 0;
      offMotion();
      unsubs.forEach((u) => u());
    };
  });
</script>

<div class="pill" class:error={errorMsg !== ""}>
  {#if errorMsg}
    <span class="message">{errorMsg}</span>
  {:else}
    <svg
      class="butterfly"
      class:finalizing={dictation === "finalizing"}
      viewBox="0 0 24 24"
      aria-hidden="true"
    >
      <g class="wing left">
        <circle cx="7.2" cy="9" r="4.8" />
        <circle cx="8.2" cy="16" r="3.6" />
      </g>
      <g class="wing right">
        <circle cx="16.8" cy="9" r="4.8" />
        <circle cx="15.8" cy="16" r="3.6" />
      </g>
      <rect class="body" x="11.15" y="7.4" width="1.7" height="10" rx="0.85" />
      <path class="antenna" d="M11.7 7.6 C11.1 5.8 10 4.6 8.8 3.9" />
      <path class="antenna" d="M12.3 7.6 C12.9 5.8 14 4.6 15.2 3.9" />
    </svg>
    <canvas bind:this={canvas} width="96" height="40"></canvas>
    <!-- Only text that says something the rest of the pill does not. The
         butterfly flaps, the waveform moves and a cue sounds the moment
         recording starts, so there is no "listening…" caption: it would repeat
         all three and cost width. The status line is shown while finalizing,
         when the waveform has stopped and nothing else reports what is
         happening. The hands-free hint is the only thing that says how to
         stop, and says only that, since the ∞ badge beside it already says
         "hands-free".

         The hint is gated on `recording`: the pill must never claim to be
         listening while it is finalizing or running a transform. Rendered
         only when it has content, because an empty wrapper still takes a flex
         slot and the pill's gap. -->
    {#if status}
      <div class="text"><span>{status}</span></div>
    {:else if dictation === "recording" && handsFree}
      <div class="text"><span class="hint">tap hotkey to finish</span></div>
    {/if}
    {#if handsFree}
      <span class="hf" title="Hands-free">∞</span>
    {/if}
  {/if}
</div>

<style>
  :global(html),
  :global(body) {
    background: transparent !important;
    overflow: hidden;
  }

  .pill {
    box-sizing: border-box;
    /* Hug the content; the transparent overlay window stays fixed-size. */
    width: fit-content;
    max-width: 404px;
    height: 56px;
    margin: 8px auto;
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 0 18px;
    border-radius: 999px;
    /* Dark in both themes — it floats over the desktop, not over the app, and
       a light capsule over arbitrary wallpaper is unreadable. What the theme
       changes is how hard it has to fight the background: in dark it lifts a
       step off black and takes a brighter rim, because on a dark desktop the
       light theme's near-black capsule loses its own edge. */
    background: var(--pill-bg);
    border: 1px solid var(--pill-border);
    color: var(--pill-fg);
    font-family: "Segoe UI Variable Text", "Segoe UI", system-ui, sans-serif;
    font-size: 14px;
  }

  .pill.error {
    border-color: var(--pill-danger-line);
    justify-content: center;
  }

  .message {
    color: var(--pill-danger);
  }

  .butterfly {
    flex: none;
    width: 24px;
    height: 24px;
    perspective: 90px;
    overflow: visible;
  }

  .butterfly .wing {
    fill: var(--pill-fg);
    /* Flap around the vertical axis through the body (view-box center). */
    transform-box: view-box;
    transform-origin: center;
    animation: 0.9s ease-in-out infinite;
  }

  .butterfly .wing.left {
    animation-name: flap-left;
  }

  .butterfly .wing.right {
    animation-name: flap-right;
  }

  .butterfly.finalizing {
    opacity: 0.7;
  }

  .butterfly.finalizing .wing {
    animation-duration: 0.45s;
  }

  .butterfly .body {
    fill: var(--pill-fg);
  }

  .butterfly .antenna {
    fill: none;
    stroke: var(--pill-fg);
    stroke-width: 1;
    stroke-linecap: round;
  }

  @keyframes flap-left {
    0%,
    100% {
      transform: rotateY(12deg);
    }
    50% {
      transform: rotateY(68deg);
    }
  }

  @keyframes flap-right {
    0%,
    100% {
      transform: rotateY(-12deg);
    }
    50% {
      transform: rotateY(-68deg);
    }
  }

  /* Reduced motion: hold the wings open rather than stopping the animation
     dead, which would snap them to an unrotated pose the design never
     intended. Spread wings still read as a butterfly — and the pill still
     reads as "recording". The waveform's own frozen state is drawResting()
     above; between them the pill says everything it said while moving. */
  @media (prefers-reduced-motion: reduce) {
    /* Matched at .wing.left / .wing.right, not .wing: the animation-name is
       set at that specificity above, and a plain `.butterfly .wing` here would
       lose to it and leave the flap running. */
    .butterfly .wing.left {
      animation: none;
      transform: rotateY(30deg);
    }

    .butterfly .wing.right {
      animation: none;
      transform: rotateY(-30deg);
    }
  }

  canvas {
    flex: none;
    width: 96px;
    height: 40px;
  }

  .text {
    /* Contribute real width so the pill hugs it. Everything shown here is a
       short fixed phrase, so it never needs to scroll. */
    flex: 0 1 auto;
    min-width: 0;
    white-space: nowrap;
    overflow: hidden;
    display: flex;
    justify-content: flex-start;
  }

  .hint {
    color: var(--pill-fg-dim);
  }

  .hf {
    flex: none;
    color: var(--pill-fg-soft);
    font-size: 16px;
  }
</style>
