// Stands in for `$lib/api` when the migration is bundled for the harness.
// `build.mjs` aliases the import to this file.
//
// `createNote` is deliberately ASYNCHRONOUS by default — it defers by a real
// macrotask before recording the call. A stub that resolves synchronously
// cannot observe re-entrancy at all: the second caller's first `await` lands
// after the first caller has already finished its whole loop, so two
// concurrent migrations look like two sequential ones and the duplicate-import
// bug hides. Scenario 8 in run-migration-checks.mjs is what needs this.

export const state = {
  calls: [],
  /** Throw from the (failAfter+1)-th call onward. */
  failAfter: Infinity,
  /** Throw for entries whose content is in this set, however often called. */
  failContent: new Set(),
  /** 0 defers by a macrotask; higher values stretch the window. */
  delayMs: 0,
};

export function reset(opts = {}) {
  state.calls = [];
  state.failAfter = opts.failAfter ?? Infinity;
  state.failContent = new Set(opts.failContent ?? []);
  state.delayMs = opts.delayMs ?? 0;
}

export async function createNote(note) {
  await new Promise((r) => setTimeout(r, state.delayMs));
  if (state.calls.length >= state.failAfter) throw new Error("simulated DB failure");
  if (state.failContent.has(note.content)) throw new Error("simulated permanent failure");
  state.calls.push(note);
  return state.calls.length;
}
