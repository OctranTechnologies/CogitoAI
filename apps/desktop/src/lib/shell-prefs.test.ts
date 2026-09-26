import { beforeEach, describe, expect, it } from "vitest";
import {
  RAIL_REGIONS,
  readStoredRailTarget,
  storeRailTarget,
} from "./shell-prefs";

/**
 * The rail's selection is the only shell state that survives a restart, so the
 * restore rules are worth asserting directly: a region is restored, a dialog
 * target is not, and an unknown value falls back rather than throwing during the
 * first render.
 */
describe("rail selection persistence", () => {
  beforeEach(() => {
    window.localStorage.clear();
  });

  it("defaults to home when nothing is stored", () => {
    expect(readStoredRailTarget()).toBe("home");
  });

  it("round-trips every persisted region", () => {
    for (const region of RAIL_REGIONS) {
      storeRailTarget(region);
      expect(readStoredRailTarget()).toBe(region);
    }
  });

  it("does not restore a dialog target, because no region corresponds to it", () => {
    for (const target of ["models", "settings"] as const) {
      storeRailTarget(target);
      expect(readStoredRailTarget()).toBe("home");
    }
  });

  it("falls back on an unknown or malformed value", () => {
    for (const value of ["", "nonsense", "../etc", "null", "42", "HOMЕ"]) {
      window.localStorage.setItem("cogitoai.rail-target", value);
      expect(readStoredRailTarget()).toBe("home");
    }
  });
});
