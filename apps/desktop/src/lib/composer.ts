import { useCallback, useEffect, useLayoutEffect, useRef } from "react";

/**
 * Grows a textarea with its content up to a ceiling, then scrolls.
 *
 * Done imperatively rather than with `field-sizing: content` so the behaviour is
 * the same on every engine the shell might run on, and so a very long prompt
 * cannot push the composer off screen.
 */
export function useAutoGrow(value: string, maxHeight = 200, minHeight = 24) {
  const ref = useRef<HTMLTextAreaElement | null>(null);

  const resize = useCallback(() => {
    const node = ref.current;
    if (!node) return;
    // Collapse first so `scrollHeight` measures the content rather than the
    // current, already-stretched, height.
    node.style.height = "auto";
    const next = Math.min(Math.max(node.scrollHeight, minHeight), maxHeight);
    node.style.height = `${next}px`;
    node.style.overflowY = node.scrollHeight > maxHeight ? "auto" : "hidden";
  }, [maxHeight, minHeight]);

  useLayoutEffect(resize, [value, resize]);

  // The composer is anchored near the bottom of a resizable window, so it also
  // has to re-measure when the viewport changes.
  useEffect(() => {
    window.addEventListener("resize", resize);
    return () => window.removeEventListener("resize", resize);
  }, [resize]);

  return ref;
}

/** Human-facing names for the runtime's execution mode identifiers. */
const MODE_LABELS: Record<string, string> = {
  read_only: "Read Only",
  safe: "Safe",
  normal: "Normal",
  auto: "Auto",
};

export function modeLabel(mode: string): string {
  return MODE_LABELS[mode] ?? mode;
}
