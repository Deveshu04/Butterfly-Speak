import { describe, expect, it } from "vitest";
import { countWords, weekStart } from "../src/quota";

describe("weekStart", () => {
  it("is the Monday of the ISO week in UTC", () => {
    expect(weekStart(new Date("2026-09-18T22:30:00Z"))).toBe("2026-09-14"); // Friday -> Monday
    expect(weekStart(new Date("2026-09-14T00:00:00Z"))).toBe("2026-09-14"); // Monday stays
    expect(weekStart(new Date("2026-09-20T23:59:59Z"))).toBe("2026-09-14"); // Sunday belongs to the same week
    expect(weekStart(new Date("2026-09-21T00:00:00Z"))).toBe("2026-09-21"); // next Monday
  });

  it("crosses month and year boundaries", () => {
    expect(weekStart(new Date("2026-01-01T12:00:00Z"))).toBe("2025-12-29"); // Thursday
    expect(weekStart(new Date("2026-03-01T00:00:00Z"))).toBe("2026-02-23"); // Sunday
  });
});

describe("countWords", () => {
  it("counts whitespace-separated words in any script", () => {
    expect(countWords("Please move the standup to Thursday.")).toBe(6);
    expect(countWords("क्या हाल है।")).toBe(3);
    expect(countWords("   ")).toBe(0);
  });

  it("ignores newlines and runs of whitespace", () => {
    expect(countWords("one\n\ntwo\t three  ")).toBe(3);
    expect(countWords("")).toBe(0);
  });
});
