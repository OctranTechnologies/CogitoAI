import { useRef, useState, type ReactNode, type PropsWithChildren } from "react";
import { cx } from "./cx";
import { useAutoFocus, useDismissable } from "./dismiss";

const SURFACE =
  "absolute z-40 min-w-44 rounded-lg border border-line-strong bg-overlay p-1 shadow-overlay animate-scale-in";

const PLACEMENT: Record<NonNullable<PopoverProps["placement"]>, string> = {
  "bottom-start": "top-full left-0 mt-1.5",
  "bottom-end": "top-full right-0 mt-1.5",
  "top-start": "bottom-full left-0 mb-1.5",
  "top-end": "bottom-full right-0 mb-1.5",
};

export interface PopoverProps extends PropsWithChildren {
  /** Rendered as the trigger. It is wrapped in a button, not cloned. */
  trigger: ReactNode;
  label: string;
  placement?: "bottom-start" | "bottom-end" | "top-start" | "top-end";
  className?: string;
  panelClassName?: string;
  render?: (close: () => void) => ReactNode;
}

/**
 * A floating surface anchored to a trigger.
 *
 * Closes on Escape and on an outside click. Deliberately not modal: the
 * interface behind stays usable, which is what makes it a popover rather than a
 * dialog. Children may be a function so the panel content is only built while
 * the surface is open.
 */
export function Popover({
  trigger,
  children,
  label,
  placement = "bottom-start",
  className,
  panelClassName,
  render,
}: PopoverProps) {
  const [open, setOpen] = useState(false);
  const { surfaceRef } = useDismissable<HTMLDivElement>(open, () => setOpen(false));
  const autoFocus = useAutoFocus<HTMLDivElement>(open);

  return (
    <div className={cx("relative inline-flex", className)}>
      <button
        type="button"
        aria-label={label}
        aria-expanded={open}
        aria-haspopup="dialog"
        className="inline-flex max-w-full items-center"
        onClick={() => setOpen((value) => !value)}
      >
        {trigger}
      </button>
      {open ? (
        <div
          ref={(node) => {
            surfaceRef(node);
            autoFocus.current = node;
          }}
          role="dialog"
          aria-label={label}
          className={cx(SURFACE, PLACEMENT[placement], panelClassName)}
        >
          {render ? render(() => setOpen(false)) : children}
        </div>
      ) : null}
    </div>
  );
}

export interface DropdownItem {
  id: string;
  label: string;
  icon?: ReactNode;
  detail?: string;
  disabled?: boolean;
  danger?: boolean;
  onSelect: () => void;
}

export interface DropdownProps extends PropsWithChildren {
  trigger: ReactNode;
  items: DropdownItem[];
  label: string;
  placement?: PopoverProps["placement"];
  className?: string;
  panelClassName?: string;
}

/** A menu of actions. Arrow keys move focus, Escape dismisses. */
export function Dropdown({
  trigger,
  items,
  label,
  placement = "bottom-start",
  className,
  panelClassName,
}: DropdownProps) {
  const [open, setOpen] = useState(false);
  const { surfaceRef } = useDismissable<HTMLDivElement>(open, () => setOpen(false));
  const listRef = useRef<HTMLDivElement | null>(null);

  function onKeyDown(event: React.KeyboardEvent<HTMLDivElement>) {
    const keys = Array.from(
      listRef.current?.querySelectorAll<HTMLButtonElement>("button:not([disabled])") ?? [],
    );
    if (keys.length === 0) return;
    const index = keys.indexOf(document.activeElement as HTMLButtonElement);
    if (event.key === "ArrowDown") {
      event.preventDefault();
      keys[(index + 1) % keys.length]?.focus();
    }
    if (event.key === "ArrowUp") {
      event.preventDefault();
      keys[(index - 1 + keys.length) % keys.length]?.focus();
    }
    if (event.key === "Home") {
      event.preventDefault();
      keys[0]?.focus();
    }
    if (event.key === "End") {
      event.preventDefault();
      keys[keys.length - 1]?.focus();
    }
  }

  return (
    <div className={cx("relative inline-flex", className)}>
      <button
        type="button"
        aria-label={label}
        aria-expanded={open}
        aria-haspopup="menu"
        className="inline-flex max-w-full items-center"
        onClick={() => setOpen((value) => !value)}
      >
        {trigger}
      </button>
      {open ? (
        <div
          ref={(node) => {
            surfaceRef(node);
            listRef.current = node;
          }}
          role="menu"
          aria-label={label}
          onKeyDown={onKeyDown}
          className={cx(SURFACE, PLACEMENT[placement], panelClassName)}
        >
          {items.map((item) => (
            <button
              key={item.id}
              type="button"
              role="menuitem"
              disabled={item.disabled}
              className={cx(
                "flex w-full items-center gap-2 rounded-md px-2 py-1.5 text-left text-sm",
                "transition-colors duration-fast",
                "disabled:pointer-events-none disabled:opacity-40",
                item.danger
                  ? "text-error hover:bg-error/10"
                  : "text-secondary hover:bg-hover hover:text-primary",
              )}
              onClick={() => {
                setOpen(false);
                item.onSelect();
              }}
            >
              {item.icon}
              <span className="min-w-0 flex-1 truncate">{item.label}</span>
              {item.detail ? (
                <span className="shrink-0 font-mono text-2xs text-faint">{item.detail}</span>
              ) : null}
            </button>
          ))}
        </div>
      ) : null}
    </div>
  );
}

export type ContextMenuItem = DropdownItem;

/** Right-click menu, flipped to stay inside the viewport near an edge. */
export function ContextMenu({
  children,
  items,
  label,
  className,
}: PropsWithChildren<{ items: ContextMenuItem[]; label: string; className?: string }>) {
  const [state, setState] = useState<{ x: number; y: number } | null>(null);
  const { surfaceRef } = useDismissable<HTMLDivElement>(state !== null, () => setState(null));

  function place(event: React.MouseEvent) {
    event.preventDefault();
    const width = 208;
    const height = items.length * 30 + 8;
    const x = Math.min(event.clientX, window.innerWidth - width - 8);
    const y = Math.min(event.clientY, window.innerHeight - height - 8);
    setState({ x: Math.max(8, x), y: Math.max(8, y) });
  }

  return (
    <div className={cx("relative", className)} onContextMenu={place}>
      {children}
      {state ? (
        <div
          ref={surfaceRef}
          role="menu"
          aria-label={label}
          className={cx("fixed z-50 min-w-52", SURFACE)}
          style={{ left: state.x, top: state.y }}
        >
          {items.map((item) => (
            <button
              key={item.id}
              type="button"
              role="menuitem"
              disabled={item.disabled}
              className={cx(
                "flex w-full items-center gap-2 rounded-md px-2 py-1.5 text-left text-sm",
                "transition-colors duration-fast",
                "disabled:pointer-events-none disabled:opacity-40",
                item.danger
                  ? "text-error hover:bg-error/10"
                  : "text-secondary hover:bg-hover hover:text-primary",
              )}
              onClick={() => {
                setState(null);
                item.onSelect();
              }}
            >
              {item.icon}
              <span className="min-w-0 flex-1 truncate">{item.label}</span>
              {item.detail ? (
                <span className="shrink-0 font-mono text-2xs text-faint">{item.detail}</span>
              ) : null}
            </button>
          ))}
        </div>
      ) : null}
    </div>
  );
}
