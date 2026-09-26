import type { RailTarget } from "../components/app-rail";

/**
 * Persisted shell preferences.
 *
 * Only the navigation rail's selection is remembered, because it is the one
 * piece of shell state a person expects to find where they left it. Everything
 * else in the shell is derived from the runtime and is re-read on launch.
 */

export const RAIL_TARGET_KEY = "cogitoai.rail-target";

/**
 * Targets that are workspace regions rather than dialogs.
 *
 * `models` and `settings` open the settings dialog, so there is no region to
 * restore them to and they are deliberately excluded from persistence.
 */
export const RAIL_REGIONS: RailTarget[] = ["home", "history", "projects", "activity"];

/**
 * Reads the stored rail target, falling back to the workspace view.
 *
 * Never throws: storage can be disabled or hold a value written by an older
 * build, and a layout that fails to render is worse than a reset selection.
 */
export function readStoredRailTarget(): RailTarget {
  try {
    const stored = window.localStorage.getItem(RAIL_TARGET_KEY);
    if (stored && (RAIL_REGIONS as string[]).includes(stored)) return stored as RailTarget;
  } catch {
    // Storage unavailable; the default view is correct.
  }
  return "home";
}

/** Records the rail selection. A no-op when storage is unavailable. */
export function storeRailTarget(target: RailTarget): void {
  try {
    window.localStorage.setItem(RAIL_TARGET_KEY, target);
  } catch {
    // The shell still works, it just forgets the view on restart.
  }
}
