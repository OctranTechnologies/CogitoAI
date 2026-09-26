import { describe, expect, it } from "vitest";
import { alpha, color, hex, resetTokenCache } from "./tokens";

/**
 * `getComputedStyle` returns nothing for a custom property that is not defined,
 * so these exercise the fallback path that a non-browser environment takes.
 * That path is the one that would silently ship a black-on-black theme.
 */
describe("design tokens", () => {
  it("falls back to the dark palette when no stylesheet is present", () => {
    resetTokenCache();
    expect(color("surface-app")).toBe("rgb(10 12 15)");
    expect(color("text-primary")).toBe("rgb(232 236 242)");
    expect(color("accent")).toBe("rgb(56 189 248)");
  });

  it("composes an alpha channel as a slash-separated colour function", () => {
    resetTokenCache();
    expect(color("accent", 0.4)).toBe("rgb(56 189 248 / 0.4)");
    expect(alpha("error", 0.07)).toBe("rgb(251 113 133 / 0.07)");
  });

  it("produces a six-digit hex triplet for Monaco tokeniser rules", () => {
    resetTokenCache();
    // Monaco rejects a colour function, so this must be bare hex.
    expect(hex("accent")).toBe("38bdf8");
    expect(hex("surface-panel")).toBe("0f1217");
    expect(hex("success")).toBe("5eead4");
  });

  it("zero-pads single-digit channels", () => {
    resetTokenCache();
    // 5 -> "05", so the triplet stays six characters.
    expect(hex("success")).toHaveLength(6);
    expect(hex("surface-app").slice(0, 2)).toBe("0a");
  });

  it("returns a usable value for an unknown token rather than undefined", () => {
    resetTokenCache();
    expect(hex("does-not-exist")).toBe("000000");
    expect(color("does-not-exist")).toBe("rgb(0 0 0)");
  });
});
