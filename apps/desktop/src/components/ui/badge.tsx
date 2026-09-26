import type { PropsWithChildren } from "react";
import { cx } from "./cx";
import { toneBorder, toneDot, toneFill, toneText, type Tone } from "./tone";

export interface BadgeProps extends PropsWithChildren {
  tone?: Tone;
  /** Dot plus text, the default for status. */
  indicator?: boolean;
  mono?: boolean;
  className?: string;
}

/** Small inline label. Used for counts, states, and rule names. */
export function Badge({ children, tone = "neutral", indicator, mono, className }: BadgeProps) {
  return (
    <span
      className={cx(
        "inline-flex items-center gap-1.5 rounded-full border px-1.5 py-0.5 text-2xs font-medium",
        "leading-none",
        mono && "font-mono",
        toneBorder(tone),
        toneFill(tone),
        toneText(tone),
        className,
      )}
    >
      {indicator ? <span className={cx("size-1.5 rounded-full", toneDot(tone))} /> : null}
      {children}
    </span>
  );
}

export interface StatusIndicatorProps {
  status: string;
  /** Renders the status text next to the dot. Off for dense rows. */
  label?: boolean;
  className?: string;
}

const BUSY: Record<string, boolean> = {
  pending: true,
  running: true,
  connecting: true,
  cancelling: true,
  canceling: true,
};

function toneForStatus(status: string): Tone {
  const value = status.trim().toLowerCase();
  if (["completed", "connected", "succeeded", "passed", "ready"].includes(value)) return "success";
  if (["failed", "error", "denied"].includes(value)) return "error";
  if (["cancelled", "canceled", "idle"].includes(value)) return "warning";
  if (BUSY[value]) return "warning";
  return "neutral";
}

/**
 * Connection and run state.
 *
 * One component for every status readout in the shell so the colour, the pulse,
 * and the wording stay consistent. The runtime owns the status string; this only
 * presents it.
 */
export function StatusIndicator({ status, label = true, className }: StatusIndicatorProps) {
  const tone = toneForStatus(status);
  const busy = BUSY[status.trim().toLowerCase()] ?? false;
  return (
    <span
      className={cx(
        "inline-flex items-center gap-1.5 text-2xs font-medium",
        toneText(tone),
        className,
      )}
    >
      <span
        aria-hidden
        className={cx("size-1.5 shrink-0 rounded-full", toneDot(tone), busy && "animate-pulse")}
      />
      {label ? <span className="capitalize">{status}</span> : <span className="sr-only">{status}</span>}
    </span>
  );
}

export function Separator({
  orientation = "horizontal",
  className,
}: {
  orientation?: "horizontal" | "vertical";
  className?: string;
}) {
  return (
    <div
      role="separator"
      aria-orientation={orientation}
      className={cx(
        "shrink-0 bg-line",
        orientation === "horizontal" ? "h-px w-full" : "h-full w-px",
        className,
      )}
    />
  );
}
