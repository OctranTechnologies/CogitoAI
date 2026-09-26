import { useEffect, type PropsWithChildren } from "react";
import { X } from "lucide-react";
import { cx } from "./cx";
import { IconButton } from "./button";
import { useDismissable } from "./dismiss";
import { Separator } from "./badge";

export interface ModalProps extends PropsWithChildren {
  open: boolean;
  onClose: () => void;
  title: string;
  description?: string;
  footer?: React.ReactNode;
  /** Tailwind max-width class. Dialogs in this shell are wide by default. */
  width?: string;
}

const FOCUSABLE =
  "button:not([disabled]), input:not([disabled]), textarea:not([disabled]), select:not([disabled]), [tabindex]:not([tabindex='-1'])";

/**
 * Modal dialog.
 *
 * Escape and backdrop click both dismiss. Tab is trapped inside so focus cannot
 * wander behind the scrim, and focus lands on the dialog when it opens. The
 * surface is located by its data attribute rather than by ref so the focus trap
 * and the dismissal hook can share one node.
 */
export function Modal({
  open,
  onClose,
  title,
  description,
  children,
  footer,
  width = "max-w-3xl",
}: ModalProps) {
  const { surfaceRef } = useDismissable<HTMLDivElement>(open, onClose);

  useEffect(() => {
    if (!open) return;
    const node = document.querySelector<HTMLDivElement>("[data-modal-surface]");
    if (node) surfaceRef(node);
  }, [open, surfaceRef]);

  useEffect(() => {
    if (!open) return;
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key !== "Tab") return;
      const node = document.querySelector<HTMLElement>("[data-modal-surface]");
      if (!node) return;
      const focusable = Array.from(node.querySelectorAll<HTMLElement>(FOCUSABLE));
      if (focusable.length === 0) return;
      const first = focusable[0];
      const last = focusable[focusable.length - 1];
      if (event.shiftKey && document.activeElement === first) {
        event.preventDefault();
        last.focus();
      } else if (!event.shiftKey && document.activeElement === last) {
        event.preventDefault();
        first.focus();
      }
    };
    document.addEventListener("keydown", onKeyDown, true);
    return () => document.removeEventListener("keydown", onKeyDown, true);
  }, [open]);

  useEffect(() => {
    if (!open) return;
    document.querySelector<HTMLElement>("[data-modal-surface]")?.focus();
  }, [open]);

  if (!open) return null;

  return (
    <div className="fixed inset-0 z-50 flex items-start justify-center overflow-y-auto p-6">
      <div className="fixed inset-0 animate-fade-in bg-scrim/70" onClick={onClose} aria-hidden />
      <div
        data-modal-surface
        role="dialog"
        aria-modal="true"
        aria-label={title}
        tabIndex={-1}
        className={cx(
          "relative my-4 flex w-full flex-col overflow-hidden rounded-xl border border-line-strong",
          "bg-panel shadow-overlay animate-slide-up outline-none",
          width,
        )}
      >
        <header className="flex items-start justify-between gap-4 px-5 py-4">
          <div className="min-w-0">
            <h2 className="text-md font-semibold text-primary">{title}</h2>
            {description ? <p className="mt-1 text-xs leading-4 text-muted">{description}</p> : null}
          </div>
          <IconButton label="Close dialog" size="sm" onClick={onClose}>
            <X className="size-icon-sm" />
          </IconButton>
        </header>
        <Separator />
        <div className="min-h-0 flex-1 overflow-y-auto">{children}</div>
        {footer ? (
          <>
            <Separator />
            <footer className="flex items-center justify-end gap-2 px-5 py-3">{footer}</footer>
          </>
        ) : null}
      </div>
    </div>
  );
}
