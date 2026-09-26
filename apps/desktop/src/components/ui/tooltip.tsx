import { useId, useState, type PropsWithChildren } from "react";
import { cx } from "./cx";

export interface TooltipProps extends PropsWithChildren {
  label: string;
  /** Where the bubble sits relative to the trigger. */
  placement?: "top" | "bottom" | "left" | "right";
  disabled?: boolean;
}

const PLACEMENT: Record<NonNullable<TooltipProps["placement"]>, string> = {
  top: "bottom-full left-1/2 -translate-x-1/2 mb-1.5",
  bottom: "top-full left-1/2 -translate-x-1/2 mt-1.5",
  left: "right-full top-1/2 -translate-y-1/2 mr-1.5",
  right: "left-full top-1/2 -translate-y-1/2 ml-1.5",
};

/**
 * Hover and focus tooltip.
 *
 * The bubble is rendered on demand rather than always present, and the trigger
 * is wrapped rather than cloned, so any element can use it without the caller
 * adding accessibility wiring. While open, `aria-describedby` points at the
 * bubble so a screen reader announces the same text.
 */
export function Tooltip({ label, children, placement = "top", disabled }: TooltipProps) {
  const [open, setOpen] = useState(false);
  const id = useId();

  if (disabled || !label) return <>{children}</>;

  return (
    <span
      className="relative inline-flex"
      onMouseEnter={() => setOpen(true)}
      onMouseLeave={() => setOpen(false)}
      onFocus={() => setOpen(true)}
      onBlur={() => setOpen(false)}
    >
      <span aria-describedby={open ? id : undefined} className="inline-flex">
        {children}
      </span>
      {open ? (
        <span
          id={id}
          role="tooltip"
          className={cx(
            "pointer-events-none absolute z-50 whitespace-nowrap rounded-md border border-line-strong",
            "bg-overlay px-2 py-1 text-2xs text-secondary shadow-overlay animate-fade-in",
            PLACEMENT[placement],
          )}
        >
          {label}
        </span>
      ) : null}
    </span>
  );
}
