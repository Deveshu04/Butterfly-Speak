// Unit tests for src/lib/notes/glyphs.ts, the glyph list the action editor
// and the Enhance menu share. Run with `pnpm test`.

import { test } from "node:test";
import assert from "node:assert/strict";
import { GLYPHS, NEW_ACTION_GLYPH, glyphFor } from "../../src/lib/notes/glyphs.ts";

test("a glyph this build can draw is kept as stored", () => {
  for (const name of GLYPHS) assert.equal(glyphFor(name), name);
});

test("an unknown or empty glyph draws the new-action glyph instead", () => {
  for (const stored of ["lightning", "", "Note", " note"]) {
    assert.equal(glyphFor(stored), NEW_ACTION_GLYPH, JSON.stringify(stored));
  }
});

test("the new-action glyph is one the editor offers", () => {
  assert.ok(GLYPHS.includes(NEW_ACTION_GLYPH));
});
