// Shared reactive stores (Svelte 5 runes).

import { getSettings, setSettings, type Settings } from "./api";

class SettingsStore {
  current = $state<Settings | null>(null);
  /** Last startup-load failure, shown by the shell instead of a blank window. */
  error = $state<string | null>(null);

  #queue: Promise<void> = Promise.resolve();

  async load() {
    this.current = await getSettings();
  }

  /** Startup load. `current` gates the entire UI, so a single failed round-trip
   * must not leave the window permanently empty: retry a few times, then keep
   * the reason so the shell can show it with a Retry button.
   */
  async loadWithRetry(attempts = 5) {
    for (let i = 0; i < attempts; i++) {
      try {
        await this.load();
        this.error = null;
        return;
      } catch (e) {
        this.error = String(e);
        console.error(`settings load failed (attempt ${i + 1}/${attempts}):`, e);
        if (i < attempts - 1) {
          await new Promise((r) => setTimeout(r, 150 * 2 ** i));
        }
      }
    }
  }

  /** Re-read what the backend has, discarding nothing of our own — for when
   * the backend wrote the settings file itself (a promoted correction) and
   * this window is holding the pre-write copy.
   *
   * Chained on the same queue as `update`: an unqueued reload landing between
   * an in-flight update's clone and its assignment would be overwritten by
   * that update a moment later, bringing the stale snapshot back.
   * Never rejects for the caller — an event handler has nowhere to put the
   * error, and a failed reload just leaves the copy we already had.
   */
  reload(): Promise<void> {
    this.reloadChecked().catch(() => {});
    return this.#queue;
  }

  /** `reload` for a caller that reports a failure: the same queued re-read,
   * but the returned promise rejects when it fails, so a page does not show
   * success while it still holds the stale copy. */
  reloadChecked(): Promise<void> {
    const run = this.#queue.then(async () => {
      this.current = await getSettings();
    });
    this.#queue = run.catch((e) => console.error("settings reload failed:", e));
    return run;
  }

  /** Apply a mutation and persist it.
   *
   * Updates are chained on an internal queue: a second update fired during
   * the first one's round-trip would otherwise clone the stale state and
   * silently revert the first edit (set_settings is a whole-object write).
   */
  update(mutate: (s: Settings) => void): Promise<void> {
    const run = this.#queue.then(async () => {
      if (!this.current) return;
      const next: Settings = JSON.parse(JSON.stringify(this.current));
      mutate(next);
      await setSettings(next);
      this.current = next;
    });
    // Keep the chain alive after a failure, but let callers see it via `run`.
    this.#queue = run.catch((e) => console.error("settings update failed:", e));
    return run;
  }
}

export const settings = new SettingsStore();

/** Transient UI state shared between nested overlays. */
class UiState {
  /** True while a shortcut recorder (the Shortcuts dialog, or the Transforms
   * editor's) is recording a chord. Anything else listening to the window
   * for keys must defer: Escape means "cancel this recording", not "close
   * the settings modal", and Ctrl+K is being recorded, not a command. */
  capturingShortcut = $state(false);
}

export const ui = new UiState();
