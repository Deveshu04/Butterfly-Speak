// The updater's state, shared by the sidebar card and the About page.

import { listen } from "@tauri-apps/api/event";
import { updateCheck, updateInstall, updateStatus } from "./api";
import { UPDATE_STATE, type UpdateStatePayload } from "./events";

/** The states the sidebar card has markup for. Narrower than the full union so
 * the card can read `.version` without re-testing what `card` already decided. */
type UpdateCardState = Extract<
  UpdateStatePayload,
  { kind: "available" | "downloading" | "installing" | "error" }
>;

/** Where a check or an install was started from. */
export type UpdateSurface = "card" | "about";

class UpdateStore {
  state = $state<UpdateStatePayload>({ kind: "idle" });
  /** "Later" hides the sidebar card for this version until the next launch;
   * the latch lives in memory only, so a restart clears it. The About page
   * still shows the update; only the nag is gone. */
  dismissed = $state<string | null>(null);
  /** Which surface started the last check or install, so an error lands where
   * the user is looking. Per-surface rather than per-verb deliberately: the
   * card's own retry is a check, and latching the verb would have made a
   * failed retry disown the card it was pressed on and move the message to a
   * page nobody had open. A failure stays where the click was. */
  lastSurface = $state<UpdateSurface | null>(null);
  #started = false;

  init() {
    if (this.#started) return;
    this.#started = true;
    updateStatus()
      .then((s) => (this.state = s))
      .catch((e) => console.error("update status failed:", e));
    listen<UpdateStatePayload>(UPDATE_STATE, (e) => (this.state = e.payload)).catch((e) =>
      console.error("update listen failed:", e),
    );
  }

  async check(surface: UpdateSurface) {
    this.lastSurface = surface;
    try {
      this.state = await updateCheck();
    } catch (e) {
      this.state = { kind: "error", message: String(e) };
    }
  }

  async install(surface: UpdateSurface) {
    this.lastSurface = surface;
    try {
      await updateInstall();
    } catch (e) {
      this.state = { kind: "error", message: String(e) };
    }
  }

  dismiss() {
    if (this.state.kind === "available") this.dismissed = this.state.version;
  }

  /** What the sidebar card draws, or `null` for no card. The state itself
   * rather than a visibility flag: a boolean cannot narrow `state` for the
   * markup, so the card would have to re-test the kinds this already ruled in.
   * An error is only ever the card's when the action that failed was started
   * here — a failed check from About stays on About, and a failed retry from
   * the card stays on the card. */
  get card(): UpdateCardState | null {
    const s = this.state;
    if (s.kind === "available") return this.dismissed === s.version ? null : s;
    if (s.kind === "error") return this.lastSurface === "card" ? s : null;
    if (s.kind === "downloading" || s.kind === "installing") return s;
    return null;
  }
}

export const update = new UpdateStore();
