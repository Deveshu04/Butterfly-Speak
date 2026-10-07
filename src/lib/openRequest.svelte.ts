// A one-shot hand-off from the command palette to the page that will show
// what was picked.
//
// The palette lives in the shell (`src/routes/+page.svelte`) and can be opened
// from anywhere, so picking a result has to do two things at once: navigate to
// the page that owns that kind of result, and tell that page which row to
// open. The shell renders `<ActiveComponent />` from a `$derived`, which means
// the destination is usually a *fresh mount* — a prop would arrive before the
// component exists, and a Tauri event would be a round trip through the
// backend for something that never leaves the webview. A module-scoped slot is
// neither: the shell fills it, then navigates, and the page drains it whether
// it was already mounted or has just come into being.
//
// The row travels whole rather than as an id. The palette already fetched it,
// so re-reading it would be a second query for the same bytes; it also keeps
// this file free of any command of its own. Nothing here is ever logged — the
// requests carry note bodies and transcript text.

import type { HistoryEntry, Note } from "$lib/api";

export type OpenRequest =
  | { kind: "note"; note: Note }
  /** `null` is the unfiled view, which is a real destination, not "no folder". */
  | { kind: "folder"; folderId: number | null }
  | { kind: "dictation"; entry: HistoryEntry };

export type OpenRequestKind = OpenRequest["kind"];

let pending = $state<OpenRequest | null>(null);

export const openRequest = {
  /** Read-only view, for a page that wants to react without consuming. */
  get pending(): OpenRequest | null {
    return pending;
  },

  /**
   * Ask whichever page owns `request.kind` to open it. Set this *before*
   * navigating: a fresh mount reads the slot on its first effect, and a slot
   * filled afterwards would be read a tick too late to matter.
   *
   * A request that nobody drains is harmless — the next one replaces it.
   */
  set(request: OpenRequest) {
    pending = request;
  },

  /**
   * Take the pending request if it is one of `kinds`, otherwise leave it
   * alone for the page that does own it.
   *
   * Call this from an `$effect`: reading `pending` subscribes the effect, so
   * the same code covers both "the page was already on screen" and "the page
   * mounted because of this request". Clearing the slot re-runs the effect
   * once more, which then finds `null` and stops — it converges rather than
   * looping.
   */
  take<K extends OpenRequestKind>(...kinds: K[]): Extract<OpenRequest, { kind: K }> | null {
    const request = pending;
    if (!request || !kinds.includes(request.kind as K)) return null;
    pending = null;
    return request as Extract<OpenRequest, { kind: K }>;
  },
};
