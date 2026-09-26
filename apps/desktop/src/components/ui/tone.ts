/**
 * Semantic tone mapping.
 *
 * Status is carried as a name (`success`, `warning`, …) rather than a colour, so
 * a component states *what* something is and this module decides how that looks.
 * The app previously repeated the same conditional class expression in four
 * places, which is how tones drift apart.
 */

export type Tone = "neutral" | "accent" | "success" | "warning" | "error";

const TONE_TEXT: Record<Tone, string> = {
  neutral: "text-muted",
  accent: "text-accent",
  success: "text-success",
  warning: "text-warning",
  error: "text-error",
};

const TONE_DOT: Record<Tone, string> = {
  neutral: "bg-faint",
  accent: "bg-accent",
  success: "bg-success",
  warning: "bg-warning",
  error: "bg-error",
};

const TONE_BORDER: Record<Tone, string> = {
  neutral: "border-line",
  accent: "border-accent/35",
  success: "border-success/35",
  warning: "border-warning/35",
  error: "border-error/35",
};

const TONE_FILL: Record<Tone, string> = {
  neutral: "bg-elevated",
  accent: "bg-accent/10",
  success: "bg-success/10",
  warning: "bg-warning/10",
  error: "bg-error/10",
};

export function toneText(tone: Tone): string {
  return TONE_TEXT[tone];
}

export function toneDot(tone: Tone): string {
  return TONE_DOT[tone];
}

export function toneBorder(tone: Tone): string {
  return TONE_BORDER[tone];
}

export function toneFill(tone: Tone): string {
  return TONE_FILL[tone];
}

/** Resolves a runtime status string onto a tone, defaulting to neutral. */
export function toneFromStatus(status: string): Tone {
  const value = status.trim().toLowerCase();
  if (["completed", "connected", "succeeded", "passed", "ready", "ok"].includes(value)) {
    return "success";
  }
  if (["failed", "error", "denied", "cancelled", "canceled"].includes(value)) return "error";
  if (["pending", "running", "connecting", "cancelling", "canceling"].includes(value)) {
    return "warning";
  }
  return "neutral";
}
