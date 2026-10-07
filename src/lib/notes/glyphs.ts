/** The glyphs a note action may wear, shared by the action editor and the
 * Enhance menu. `notes::actions` has a test that reads this file, so the
 * Rust side and the webview agree on what can be drawn. */

/** What a new action starts with: `notes::actions::DEFAULT_GLYPH`. */
export const NEW_ACTION_GLYPH = "note";

/** Every glyph the action editor offers, each one drawn by `Icon.svelte`. The
 * list is fixed because `Icon` renders an unknown name as an empty 24px box
 * rather than falling back, so a row whose stored glyph isn't one of these
 * would silently lose its picture. */
export const GLYPHS: readonly string[] = [
  "note",
  "bullets",
  "book",
  "type",
  "scissors",
  "check",
  "clock",
  "sparkles",
];

/** The stored glyph if it is one this build can draw, the new-action glyph
 * otherwise — so an action written by an older or newer version still shows
 * something. */
export const glyphFor = (name: string): string =>
  GLYPHS.includes(name) ? name : NEW_ACTION_GLYPH;
