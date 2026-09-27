import { describe, expect, it } from "vitest";
import { desktopShortcutLabel, matchesDesktopShortcut, paletteShortcutLabel } from "./keyboard";

function key(
  key: string,
  options: Partial<Pick<KeyboardEvent, "code" | "ctrlKey" | "metaKey" | "shiftKey">> = {},
): KeyboardEvent {
  return { key, code: options.code ?? "", ctrlKey: options.ctrlKey ?? false, metaKey: options.metaKey ?? false, shiftKey: options.shiftKey ?? false } as KeyboardEvent;
}

describe("desktop shortcuts", () => {
  it("distinguishes session search from the alternate command palette shortcut", () => {
    expect(matchesDesktopShortcut(key("p", { ctrlKey: true }), "searchSessions")).toBe(true);
    expect(matchesDesktopShortcut(key("p", { metaKey: true }), "searchSessions")).toBe(true);
    expect(matchesDesktopShortcut(key("p", { ctrlKey: true, shiftKey: true }), "searchSessions")).toBe(false);
    expect(matchesDesktopShortcut(key("p", { ctrlKey: true, shiftKey: true }), "paletteAlternate")).toBe(true);
  });

  it("recognizes palette, new task, prompt, and terminal shortcuts", () => {
    expect(matchesDesktopShortcut(key("k", { ctrlKey: true }), "palette")).toBe(true);
    expect(matchesDesktopShortcut(key("n", { metaKey: true }), "newTask")).toBe(true);
    expect(matchesDesktopShortcut(key("Enter", { ctrlKey: true }), "submitPrompt")).toBe(true);
    expect(matchesDesktopShortcut(key("`", { ctrlKey: true, code: "Backquote" }), "toggleTerminal")).toBe(true);
  });

  it("shows platform-appropriate shortcut hints", () => {
    expect(desktopShortcutLabel("newTask", "Win32")).toBe("Ctrl+N");
    expect(desktopShortcutLabel("newTask", "MacIntel")).toBe("⌘N");
    expect(paletteShortcutLabel("MacIntel")).toBe("⌘K / ⌘Shift+P");
  });
});
