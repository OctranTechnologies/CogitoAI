import { useRef } from "react";
import {
  Activity,
  Bot,
  CircleHelp,
  Cpu,
  FolderGit2,
  Home,
  Settings2,
  Timer,
  type LucideIcon,
} from "lucide-react";
import { IconButton, Tooltip, cx } from "./ui";

/**
 * The views the rail can select.
 *
 * `models` and `settings` are not workspace views; they open the settings dialog
 * on a given screen. They are part of the same navigation so the rail reads as
 * one list rather than two.
 */
export type RailTarget = "home" | "history" | "projects" | "activity" | "models" | "settings";

interface RailEntry {
  target: RailTarget;
  label: string;
  icon: LucideIcon;
}

const PRIMARY: RailEntry[] = [
  { target: "home", label: "Home", icon: Home },
  { target: "history", label: "Sessions and history", icon: Timer },
  { target: "projects", label: "Projects", icon: FolderGit2 },
  { target: "activity", label: "Agent activity", icon: Activity },
  { target: "models", label: "Models and integrations", icon: Cpu },
  { target: "settings", label: "Settings", icon: Settings2 },
];

export interface AppRailProps {
  target: RailTarget;
  onSelect: (target: RailTarget) => void;
  /** Rendered under the primary entries, above help. */
  footer?: React.ReactNode;
  /** Short status line, e.g. the connection dot. */
  statusSlot?: React.ReactNode;
  profileLabel: string;
}

/**
 * Narrow, fixed navigation rail.
 *
 * Full height and non-scrolling: it is a frame around the application, not a
 * region that reflows. Entries are icon-only, so every one carries a tooltip and
 * an accessible name. Up and Down move between entries so the rail is reachable
 * without a pointer.
 */
export function AppRail({
  target,
  onSelect,
  footer,
  statusSlot,
  profileLabel,
}: AppRailProps) {
  const listRef = useRef<HTMLDivElement | null>(null);

  function onKeyDown(event: React.KeyboardEvent<HTMLDivElement>) {
    const keys = ["ArrowDown", "ArrowUp", "Home", "End"];
    if (!keys.includes(event.key)) return;
    const buttons = Array.from(
      listRef.current?.querySelectorAll<HTMLButtonElement>("button[data-rail-entry]") ?? [],
    );
    if (buttons.length === 0) return;
    event.preventDefault();
    const index = buttons.indexOf(document.activeElement as HTMLButtonElement);
    if (event.key === "ArrowDown") buttons[(index + 1) % buttons.length]?.focus();
    if (event.key === "ArrowUp") {
      buttons[(index - 1 + buttons.length) % buttons.length]?.focus();
    }
    if (event.key === "Home") buttons[0]?.focus();
    if (event.key === "End") buttons[buttons.length - 1]?.focus();
  }

  return (
    <nav
      aria-label="Primary"
      className="flex w-[52px] shrink-0 flex-col items-center border-r border-line bg-app py-2"
    >
      {statusSlot ? <div className="mb-2 flex h-6 items-center">{statusSlot}</div> : null}

      <div
        ref={listRef}
        role="tablist"
        aria-orientation="vertical"
        onKeyDown={onKeyDown}
        className="flex w-full flex-1 flex-col items-center gap-0.5"
      >
        {PRIMARY.map((entry) => {
          const Icon = entry.icon;
          const selected = target === entry.target;
          return (
            <Tooltip key={entry.target} label={entry.label} placement="right">
              <button
                type="button"
                role="tab"
                data-rail-entry
                aria-selected={selected}
                aria-label={entry.label}
                onClick={() => onSelect(entry.target)}
                className={cx(
                  "flex size-9 items-center justify-center rounded-md",
                  "transition-colors duration-fast",
                  selected
                    ? "bg-elevated text-primary"
                    : "text-muted hover:bg-hover hover:text-primary",
                )}
              >
                <Icon className="size-icon-lg" />
              </button>
            </Tooltip>
          );
        })}
      </div>

      {footer ? <div className="mt-2 flex w-full flex-col items-center gap-0.5">{footer}</div> : null}

      <div className="mt-1 flex flex-col items-center gap-0.5">
        <Tooltip label="Help" placement="right">
          <IconButton
            label="Help"
            size="md"
            className="size-9 rounded-md"
            onClick={() => onSelect("settings")}
          >
            <CircleHelp className="size-icon-lg" />
          </IconButton>
        </Tooltip>
        <Tooltip label={profileLabel} placement="right">
          <span
            aria-label={profileLabel}
            role="img"
            className="flex size-8 cursor-default items-center justify-center rounded-full border border-line bg-elevated text-2xs font-semibold uppercase text-secondary"
          >
            <Bot className="size-icon-sm" />
          </span>
        </Tooltip>
      </div>
    </nav>
  );
}
