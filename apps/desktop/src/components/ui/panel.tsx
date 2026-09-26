import type { PropsWithChildren, ReactNode } from "react";
import { cx } from "./cx";
import { Separator } from "./badge";

export interface PanelProps extends PropsWithChildren {
  /** Where the panel sits visually: flush against the window, or raised. */
  elevation?: "flat" | "raised";
  className?: string;
}

/** A bordered surface. The only container primitive, so edges stay uniform. */
export function Panel({ children, elevation = "flat", className }: PanelProps) {
  return (
    <section
      className={cx(
        "rounded-lg border border-line",
        elevation === "raised" ? "bg-elevated shadow-panel" : "bg-panel",
        className,
      )}
    >
      {children}
    </section>
  );
}

export interface PanelHeaderProps {
  title: ReactNode;
  description?: ReactNode;
  actions?: ReactNode;
  icon?: ReactNode;
  className?: string;
}

export function PanelHeader({
  title,
  description,
  actions,
  icon,
  className,
}: PanelHeaderProps) {
  return (
    <header className={cx("flex items-start justify-between gap-3 border-b border-line px-3 py-2.5", className)}>
      <div className="min-w-0">
        <div className="flex items-center gap-1.5">
          {icon}
          <h2 className="truncate text-sm font-semibold text-primary">{title}</h2>
        </div>
        {description ? <p className="mt-1 text-xs leading-4 text-muted">{description}</p> : null}
      </div>
      {actions ? <div className="flex shrink-0 items-center gap-1">{actions}</div> : null}
    </header>
  );
}

export function PanelSection({ children, className }: PropsWithChildren<{ className?: string }>) {
  return <div className={cx("space-y-2", className)}>{children}</div>;
}

export function ScrollArea({
  children,
  className,
  ...rest
}: PropsWithChildren<{ className?: string }> & React.HTMLAttributes<HTMLDivElement>) {
  return (
    <div className={cx("scroll-area", className)} {...rest}>
      {children}
    </div>
  );
}

/** Dashed placeholder used where a list has no entries yet. */
export function EmptyState({ children, className }: PropsWithChildren<{ className?: string }>) {
  return (
    <p
      className={cx(
        "rounded-md border border-dashed border-line px-3 py-2.5 text-xs leading-5 text-faint",
        className,
      )}
    >
      {children}
    </p>
  );
}

export { Separator };
