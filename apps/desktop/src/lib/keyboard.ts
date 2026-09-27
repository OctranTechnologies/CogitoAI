/** Shared keyboard and command metadata for the desktop shell. */
export type DesktopShortcutId =
  | "palette"
  | "paletteAlternate"
  | "newTask"
  | "searchSessions"
  | "submitPrompt"
  | "toggleTerminal";

interface ShortcutDefinition {
  key: string;
  code?: string;
  shift?: boolean;
}

export const DESKTOP_SHORTCUTS: Record<DesktopShortcutId, ShortcutDefinition> = {
  palette: { key: "k" },
  paletteAlternate: { key: "p", shift: true },
  newTask: { key: "n" },
  searchSessions: { key: "p" },
  submitPrompt: { key: "enter" },
  toggleTerminal: { key: "`", code: "Backquote" },
};

export interface DesktopCommandDefinition {
  id: string;
  label: string;
  group: string;
  keywords: string;
  shortcut?: DesktopShortcutId;
}

/** Palette labels live here so help, hints, and handlers share one vocabulary. */
export const DESKTOP_COMMANDS = [
  { id: "session.new", label: "New task", group: "Tasks", keywords: "session prompt create", shortcut: "newTask" },
  { id: "workspace.open", label: "Open project or workspace", group: "Projects", keywords: "folder repository choose" },
  { id: "sessions.search", label: "Search projects and sessions", group: "Sessions", keywords: "filter find history", shortcut: "searchSessions" },
  { id: "session.resume", label: "Resume active session", group: "Sessions", keywords: "continue task" },
  { id: "model.switch", label: "Switch model…", group: "Runtime", keywords: "provider settings" },
  { id: "mode.switch", label: "Switch execution mode…", group: "Runtime", keywords: "permissions safe auto read-only" },
  { id: "view.diff", label: "Show diff", group: "Workspace", keywords: "changes files inspect" },
  { id: "terminal.open", label: "Open terminal", group: "Workspace", keywords: "shell console", shortcut: "toggleTerminal" },
  { id: "view.checkpoints", label: "Show checkpoints", group: "Workspace", keywords: "history restore recovery" },
  { id: "change.undo", label: "Undo latest harness change", group: "Workspace", keywords: "restore checkpoint revert" },
  { id: "settings.open", label: "Open settings", group: "Preferences", keywords: "configuration" },
] as const satisfies readonly DesktopCommandDefinition[];

export type DesktopCommandId = typeof DESKTOP_COMMANDS[number]["id"];

type KeyboardLike = Pick<KeyboardEvent, "key" | "code" | "ctrlKey" | "metaKey" | "shiftKey">;

/** Ctrl and Command are treated as the platform's primary modifier. */
export function matchesDesktopShortcut(event: KeyboardLike, id: DesktopShortcutId): boolean {
  const definition = DESKTOP_SHORTCUTS[id];
  if (!(event.ctrlKey || event.metaKey) || event.shiftKey !== Boolean(definition.shift)) return false;
  if (definition.code && event.code === definition.code) return true;
  return event.key.toLowerCase() === definition.key;
}

function isMacPlatform(platform?: string): boolean {
  const value = platform ?? (typeof navigator === "undefined" ? "" : navigator.platform);
  return /Mac|iPhone|iPad/i.test(value);
}

export function desktopShortcutLabel(id: DesktopShortcutId, platform?: string): string {
  const modifier = isMacPlatform(platform) ? "⌘" : "Ctrl+";
  switch (id) {
    case "palette": return `${modifier}K`;
    case "paletteAlternate": return `${modifier}Shift+P`;
    case "newTask": return `${modifier}N`;
    case "searchSessions": return `${modifier}P`;
    case "submitPrompt": return `${modifier}Enter`;
    case "toggleTerminal": return `${modifier}\``;
  }
}

export function paletteShortcutLabel(platform?: string): string {
  return `${desktopShortcutLabel("palette", platform)} / ${desktopShortcutLabel("paletteAlternate", platform)}`;
}
