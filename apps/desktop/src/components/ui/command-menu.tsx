import { useEffect, useMemo, useState, type ReactNode } from "react";
import { Search } from "lucide-react";
import { cx } from "./cx";
import { useDismissable } from "./dismiss";

export interface CommandItem {
  id: string;
  label: string;
  group?: string;
  detail?: string;
  keywords?: string;
  icon?: ReactNode;
  disabled?: boolean;
  onSelect: () => void;
}

export interface CommandMenuProps {
  open: boolean;
  onClose: () => void;
  items: CommandItem[];
  label: string;
  placeholder?: string;
  emptyMessage?: string;
}

/**
 * Fuzzy command palette.
 *
 * Subsequence match rather than a full ranking algorithm: it is predictable,
 * needs no dependency, and is ample for a list of shell actions. Arrow keys move
 * the highlight, Enter runs it, Escape dismisses.
 */
export function CommandMenu({
  open,
  onClose,
  items,
  label,
  placeholder = "Type a command…",
  emptyMessage = "No matching command.",
}: CommandMenuProps) {
  const [query, setQuery] = useState("");
  const [active, setActive] = useState(0);
  const { surfaceRef } = useDismissable<HTMLDivElement>(open, onClose);

  useEffect(() => {
    if (open) {
      setQuery("");
      setActive(0);
    }
  }, [open]);

  const matches = useMemo(() => {
    const needle = query.trim().toLowerCase();
    if (!needle) return items;
    return items.filter((item) => {
      const haystack = `${item.label} ${item.group ?? ""} ${item.keywords ?? ""}`.toLowerCase();
      return isSubsequence(needle, haystack);
    });
  }, [items, query]);

  useEffect(() => {
    if (active >= matches.length) setActive(0);
  }, [matches.length, active]);

  if (!open) return null;

  function onKeyDown(event: React.KeyboardEvent<HTMLDivElement>) {
    if (event.key === "ArrowDown") {
      event.preventDefault();
      setActive((value) => (matches.length === 0 ? 0 : (value + 1) % matches.length));
    } else if (event.key === "ArrowUp") {
      event.preventDefault();
      setActive((value) => (matches.length === 0 ? 0 : (value - 1 + matches.length) % matches.length));
    } else if (event.key === "Enter") {
      event.preventDefault();
      const item = matches[active];
      if (item && !item.disabled) {
        onClose();
        item.onSelect();
      }
    }
  }

  return (
    <div className="fixed inset-0 z-50 flex items-start justify-center p-4 pt-[12vh]">
      <div className="fixed inset-0 animate-fade-in bg-scrim/70" onClick={onClose} aria-hidden />
      <div
        ref={surfaceRef}
        role="dialog"
        aria-modal="true"
        aria-label={label}
        onKeyDown={onKeyDown}
        className="relative flex w-full max-w-lg flex-col overflow-hidden rounded-xl border border-line-strong bg-overlay shadow-overlay animate-slide-up"
      >
        <div className="flex items-center gap-2 border-b border-line px-3">
          <Search className="size-icon-sm shrink-0 text-faint" />
          <input
            data-autofocus
            value={query}
            onChange={(event) => setQuery(event.target.value)}
            placeholder={placeholder}
            aria-label={label}
            className="h-10 min-w-0 flex-1 bg-transparent text-sm text-primary outline-none placeholder:text-faint"
          />
        </div>
        <div className="max-h-80 overflow-y-auto p-1" role="listbox" aria-label={label}>
          {matches.length === 0 ? (
            <p className="px-2 py-6 text-center text-xs text-faint">{emptyMessage}</p>
          ) : (
            matches.map((item, index) => (
              <button
                key={item.id}
                type="button"
                role="option"
                aria-selected={index === active}
                disabled={item.disabled}
                onMouseEnter={() => setActive(index)}
                onClick={() => {
                  onClose();
                  item.onSelect();
                }}
                className={cx(
                  "flex w-full items-center gap-2 rounded-md px-2 py-1.5 text-left text-sm",
                  "transition-colors duration-fast",
                  "disabled:pointer-events-none disabled:opacity-40",
                  index === active
                    ? "bg-active text-primary"
                    : "text-secondary hover:bg-hover",
                )}
              >
                {item.icon}
                <span className="min-w-0 flex-1 truncate">{item.label}</span>
                {item.detail ? (
                  <span className="shrink-0 font-mono text-2xs text-faint">{item.detail}</span>
                ) : null}
                {item.group ? (
                  <span className="label-mono shrink-0 normal-case">{item.group}</span>
                ) : null}
              </button>
            ))
          )}
        </div>
      </div>
    </div>
  );
}

function isSubsequence(needle: string, haystack: string): boolean {
  let index = 0;
  for (const character of haystack) {
    if (character === needle[index]) index += 1;
    if (index === needle.length) return true;
  }
  return needle.length === 0;
}
