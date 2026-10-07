// Light / dark / auto, and the machinery that keeps every window agreeing.
//
// The preference lives in localStorage, NOT in the settings file, and that is
// a deliberate exception to "preferences live in Settings":
//
//   * It has to be readable *synchronously, before first paint*. The pre-paint
//     script in src/app.html stamps `.dark` on <html> straight from this key.
//     Settings arrive over an async IPC round-trip after the window is already
//     on screen — the shell even renders a boot screen while `settings.current`
//     is null — so a settings-backed theme is a guaranteed white flash.
//   * The overlay window never loads settings at all. It listens for events and
//     draws a pill; giving it a settings round-trip just to know its own colour
//     would be a lot of machinery for a colour.
//
// The cost is that the preference is per-WebView2-profile rather than part of
// the exported settings JSON, so Export/Import does not carry it. That is the
// right trade for something the user re-picks in two clicks.

import { emit, listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { persistResolvedTheme } from "$lib/api";

export type ThemePref = "light" | "dark" | "auto";
export type ResolvedTheme = "light" | "dark";

/** localStorage key. The pre-paint script in src/app.html reads the same key
 *  with the same validation — change them together. */
const KEY = "theme";

/** Frontend-to-frontend broadcast, so the overlay repaints the moment the
 *  setting changes in the main window.
 *
 *  Not in $lib/events.ts: that file is the contract with src-tauri/src/events.rs
 *  and every name in it has a Rust counterpart. Nothing in Rust emits or
 *  listens for this one.
 *
 *  A `storage` event would in principle do the same job for free, but the two
 *  windows are separate WebView2 controllers and cross-controller storage
 *  notification is not something Tauri promises. It is subscribed to anyway as
 *  a belt-and-braces path — applying a theme twice costs nothing. */
const THEME_EVENT = "theme://changed";

const DARK_MQ = "(prefers-color-scheme: dark)";

function readPref(): ThemePref {
  try {
    const v = localStorage.getItem(KEY);
    if (v === "light" || v === "dark" || v === "auto") return v;
  } catch {
    // Storage disabled or unavailable (also: prerender, where there is no
    // localStorage at all). Fall through to the default.
  }
  return "auto";
}

function systemPrefersDark(): boolean {
  if (typeof window === "undefined" || !window.matchMedia) return false;
  return window.matchMedia(DARK_MQ).matches;
}

class ThemeStore {
  #pref = $state<ThemePref>(readPref());
  /** What the OS is asking for. Only consulted while `pref` is "auto", but
   *  kept current regardless so flipping back to auto is instant. */
  #system = $state<ResolvedTheme>(systemPrefersDark() ? "dark" : "light");

  /** Whether this window is the one that mirrors the theme to Rust. Only the
   *  main window may: `capabilities/overlay.json` grants the pill no app
   *  command, so an overlay call would be an ACL rejection in the console
   *  rather than a second writer. Both windows resolve the same theme, so one
   *  writer is all there is to have. Not `$state` — it is a fact about the
   *  window, fixed for its lifetime, and nothing renders from it. */
  #mirrors = false;

  /** The last value the sidecar was told, so the repaints that do *not*
   *  change the resolved theme (init, a hot reload, an OS flip while the
   *  preference is explicit) cost no IPC. Reset to null on a failed write so
   *  the next change retries rather than believing a lie. */
  #mirrored: ResolvedTheme | null = null;

  get pref(): ThemePref {
    return this.#pref;
  }

  get resolved(): ResolvedTheme {
    return this.#pref === "auto" ? this.#system : this.#pref;
  }

  /** Set from the UI: persist, paint, and tell the other window. */
  set(pref: ThemePref) {
    if (pref === this.#pref) return;
    this.#pref = pref;
    try {
      localStorage.setItem(KEY, pref);
    } catch (e) {
      // A theme that does not survive a restart still beats one that throws
      // on the way to being applied.
      console.error("theme: could not persist preference:", e);
    }
    this.#apply();
    emit(THEME_EVENT, { pref }).catch((e) =>
      console.error("theme: could not broadcast to other windows:", e),
    );
  }

  /** Adopt a preference that arrived from elsewhere (the other window, or a
   *  storage event). Deliberately does not re-broadcast: two windows echoing
   *  each other is a loop. */
  #adopt(pref: ThemePref) {
    if (pref === this.#pref) return;
    this.#pref = pref;
    this.#apply();
  }

  #apply() {
    if (typeof document === "undefined") return;
    const root = document.documentElement;
    const dark = this.resolved === "dark";
    root.classList.toggle("dark", dark);
    // app.html's pre-paint script writes this inline, which outranks the
    // stylesheet's `color-scheme` for the life of the document — so runtime
    // changes have to keep writing it too, or the native scrollbars and form
    // controls stay stuck on whichever theme booted.
    root.style.colorScheme = dark ? "dark" : "light";
    this.#mirror();
  }

  /** Tell Rust what this window resolved to, so the *next* launch can fill the
   *  window before the webview paints.
   *
   *  This is the only part of the theme that leaves localStorage, and it is a
   *  one-way hint rather than a second source of truth: nothing here ever
   *  reads it back. `setup()` runs before any webview exists, so it cannot ask
   *  the window what theme it is about to be; without this it would fall back to
   *  the OS theme, which is wrong for exactly the users who overrode it. */
  #mirror() {
    if (!this.#mirrors) return;
    const resolved = this.resolved;
    if (resolved === this.#mirrored) return;
    this.#mirrored = resolved;
    persistResolvedTheme(resolved).catch((e) => {
      // Costs one pre-paint frame of the wrong colour at the next launch,
      // so it is logged and dropped, never surfaced.
      this.#mirrored = null;
      console.error("theme: could not persist the resolved theme:", e);
    });
  }

  /** Subscribe to everything that can change the resolved theme behind our
   *  back. Called once per window, from the root layout. Returns a teardown. */
  init(): () => void {
    // Decided before the first `#apply`, which is what performs the mirror.
    // `getCurrentWindow` reads the label out of `__TAURI_INTERNALS__.metadata`
    // with no IPC, so it needs no capability of its own — but it throws
    // outside a Tauri webview (a plain `pnpm dev` browser tab), where there is
    // nothing to mirror to anyway.
    try {
      this.#mirrors = getCurrentWindow().label === "main";
    } catch {
      this.#mirrors = false;
    }

    // The pre-paint script has already stamped the class; re-applying is a
    // no-op that also covers a dev-time hot reload.
    this.#apply();

    const offs: Array<() => void> = [];

    if (typeof window !== "undefined" && window.matchMedia) {
      const mq = window.matchMedia(DARK_MQ);
      const onSystem = (e: MediaQueryListEvent) => {
        this.#system = e.matches ? "dark" : "light";
        // Only "auto" follows the OS, but #system is tracked either way, so
        // the repaint is gated here rather than by unsubscribing: a listener
        // that has to be torn down and re-subscribed every time the user
        // touches the setting is a lifecycle bug waiting to happen.
        if (this.#pref === "auto") this.#apply();
      };
      mq.addEventListener("change", onSystem);
      offs.push(() => mq.removeEventListener("change", onSystem));

      const onStorage = (e: StorageEvent) => {
        if (e.key !== KEY) return;
        this.#adopt(readPref());
      };
      window.addEventListener("storage", onStorage);
      offs.push(() => window.removeEventListener("storage", onStorage));
    }

    let stopped = false;
    listen<{ pref: ThemePref }>(THEME_EVENT, (e) => this.#adopt(e.payload.pref))
      .then((un) => {
        if (stopped) un();
        else offs.push(un);
      })
      .catch((e) => console.error("theme: could not subscribe to changes:", e));

    return () => {
      stopped = true;
      offs.forEach((off) => off());
    };
  }
}

export const theme = new ThemeStore();

/** Read a colour token off the document root.
 *
 *  For the one place a colour cannot be written in CSS: the overlay's waveform
 *  is painted into a canvas, and `ctx.fillStyle` needs a literal. Callers must
 *  re-read when `theme.resolved` changes — the canvas has no way to notice a
 *  custom property moving under it. */
export function colorToken(name: string, fallback: string): string {
  if (typeof document === "undefined") return fallback;
  const v = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  return v || fallback;
}

/** True when the OS asks for reduced motion, tracked live. Anything that
 *  animates to convey state should freeze in a state that still conveys it —
 *  never simply disappear. */
export function prefersReducedMotion(): boolean {
  if (typeof window === "undefined" || !window.matchMedia) return false;
  return window.matchMedia("(prefers-reduced-motion: reduce)").matches;
}

/** Subscribe to reduced-motion changes. Returns a teardown. */
export function onReducedMotionChange(fn: (reduce: boolean) => void): () => void {
  if (typeof window === "undefined" || !window.matchMedia) return () => {};
  const mq = window.matchMedia("(prefers-reduced-motion: reduce)");
  const handler = (e: MediaQueryListEvent) => fn(e.matches);
  mq.addEventListener("change", handler);
  return () => mq.removeEventListener("change", handler);
}
