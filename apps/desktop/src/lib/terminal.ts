/**
 * Types and helpers for human-operated terminal sessions.
 *
 * A terminal here is interactive and **not** governed by the agent policy
 * engine. See the security note in `store.ts` and the runtime documentation for
 * why that boundary is deliberate.
 */

export interface PtyInfo {
  id: string;
  program: string;
  working_directory: string;
  /** Always `"human"`; the runtime rejects any other origin. */
  origin: string;
  cols: number;
  rows: number;
  pid: number | null;
}

export type TerminalExitReason = "exited" | "closed";

export interface TerminalExit {
  terminalId: string;
  exitCode: number | null;
  reason: TerminalExitReason;
}

export const DEFAULT_TERMINAL_COLS = 80;
export const DEFAULT_TERMINAL_ROWS = 24;

/** The runtime requires this exact origin; anything else is refused. */
export const HUMAN_ORIGIN = "human" as const;

export function isPtyInfo(value: unknown): value is PtyInfo {
  if (!value || typeof value !== "object") return false;
  const info = value as Partial<PtyInfo>;
  return (
    typeof info.id === "string" &&
    typeof info.program === "string" &&
    typeof info.working_directory === "string" &&
    typeof info.origin === "string" &&
    typeof info.cols === "number" &&
    typeof info.rows === "number"
  );
}

/** Describes a terminal that has ended, in a form suitable for the UI. */
export function describeExit(exit: TerminalExit): string {
  if (exit.reason === "closed") {
    return "Terminal closed.";
  }
  return exit.exitCode === null
    ? "Terminal ended."
    : `Terminal exited with code ${exit.exitCode}.`;
}
