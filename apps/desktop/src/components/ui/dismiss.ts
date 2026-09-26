import { useCallback, useEffect, useRef } from "react";

/**
 * Shared dismissal behaviour for every floating surface.
 *
 * Popover, dropdown, context menu, and modal all need the same three things:
 * close on Escape, close on an outside click, and return focus to whatever
 * opened them. Implementing that once keeps the keyboard contract identical
 * across the shell.
 */
export function useDismissable<T extends HTMLElement>(open: boolean, onDismiss: () => void) {
  const surface = useRef<T | null>(null);
  const trigger = useRef<HTMLElement | null>(null);
  const dismiss = useRef(onDismiss);
  dismiss.current = onDismiss;

  useEffect(() => {
    if (!open) return;
    // Remember the opener before anything moves focus.
    trigger.current = (document.activeElement as HTMLElement | null) ?? null;

    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.stopPropagation();
        dismiss.current();
      }
    };
    const onPointerDown = (event: MouseEvent) => {
      const target = event.target as Node | null;
      if (!target) return;
      if (surface.current?.contains(target)) return;
      if (trigger.current?.contains(target)) return;
      dismiss.current();
    };

    document.addEventListener("keydown", onKeyDown, true);
    document.addEventListener("mousedown", onPointerDown, true);
    return () => {
      document.removeEventListener("keydown", onKeyDown, true);
      document.removeEventListener("mousedown", onPointerDown, true);
    };
  }, [open]);

  // Restore focus to the opener so keyboard users are not dropped at the top of
  // the document after closing.
  useEffect(() => {
    if (open) return;
    const previous = trigger.current;
    if (previous && document.body.contains(previous)) {
      previous.focus?.();
    }
    trigger.current = null;
  }, [open]);

  return {
    surfaceRef: useCallback((node: T | null) => {
      surface.current = node;
    }, []),
  };
}

/** Closes when focus leaves the subtree entirely, for non-modal menus. */
export function useFocusWithin<T extends HTMLElement>(active: boolean, onExit: () => void) {
  const container = useRef<T | null>(null);
  const handler = useRef(onExit);
  handler.current = onExit;

  useEffect(() => {
    if (!active) return;
    const node = container.current;
    if (!node) return;
    const onFocusIn = (event: FocusEvent) => {
      const target = event.target as Node | null;
      if (target && !node.contains(target)) handler.current();
    };
    document.addEventListener("focusin", onFocusIn, true);
    return () => document.removeEventListener("focusin", onFocusIn, true);
  }, [active]);

  return container;
}

/** Moves focus into a surface once it opens, for keyboard entry. */
export function useAutoFocus<T extends HTMLElement>(active: boolean) {
  const surface = useRef<T | null>(null);
  useEffect(() => {
    if (!active) return;
    const node = surface.current;
    if (!node) return;
    const target = node.querySelector<HTMLElement>(
      "[data-autofocus], input, button, [tabindex]:not([tabindex='-1'])",
    );
    target?.focus();
  }, [active]);
  return surface;
}
