import type { ReactNode } from "react";
import { cx } from "./ui";

export interface ComposerShellProps {
  children: ReactNode;
  /** The landing composer is larger and sits lower on the screen. */
  size?: "compact" | "landing";
  className?: string;
}

/** The raised, rounded surface that holds the textarea and the control row. */
export function ComposerShell({ children, size = "compact", className }: ComposerShellProps) {
  return (
    <div
      className={cx(
        "rounded-xl border border-line bg-elevated shadow-overlay",
        "transition-colors duration-fast focus-within:border-line-stronger",
        size === "landing" ? "p-3" : "p-2",
        className,
      )}
    >
      {children}
    </div>
  );
}
