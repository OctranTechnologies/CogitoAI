import { GitBranch, Loader, Radio, ShieldCheck, Cpu } from "lucide-react";
import type { ModelSettings, PermissionSettings } from "../lib/settings";
import type { RunPhase } from "../lib/events";
import { modeLabel } from "../lib/composer";
import { cx } from "./ui";

/**
 * A single quiet line of session context.
 *
 * Every field is shown only when the runtime actually reported it. An absent
 * branch is left out rather than rendered as a placeholder, so the header never
 * claims to know something it does not.
 */
export function SessionHeader({
  projectName,
  branch,
  connected,
  runPhase,
  models,
  permissions,
  activeTools,
  failures,
}: {
  projectName: string | null;
  branch: string | null;
  connected: boolean;
  runPhase: RunPhase;
  models: ModelSettings | null;
  permissions: PermissionSettings | null;
  activeTools: number;
  failures: number;
}) {
  const busy = runPhase === "pending" || runPhase === "running" || runPhase === "cancelling";

  return (
    <header className="flex shrink-0 flex-wrap items-center gap-x-3 gap-y-1 border-b border-line px-4 py-1.5 text-2xs text-faint">
      {projectName ? (
        <span className="truncate font-medium text-secondary" data-testid="header-project">
          {projectName}
        </span>
      ) : null}

      {branch ? (
        <span className="inline-flex min-w-0 items-center gap-1" data-testid="header-branch">
          <GitBranch className="size-icon-xs shrink-0 opacity-70" />
          <span className="truncate">{branch}</span>
        </span>
      ) : null}

      <span
        className={cx("inline-flex items-center gap-1", connected ? "text-success" : "text-error")}
        data-testid="header-runtime"
      >
        {busy ? <Loader className="size-icon-xs animate-spin" /> : <Radio className="size-icon-xs" />}
        {connected ? (busy ? "running" : "connected") : "disconnected"}
      </span>

      {models ? (
        <span className="inline-flex min-w-0 items-center gap-1" data-testid="header-model">
          <Cpu className="size-icon-xs shrink-0 opacity-70" />
          <span className="truncate">
            {models.provider}/{models.model}
          </span>
        </span>
      ) : null}

      {permissions ? (
        <span className="inline-flex items-center gap-1" data-testid="header-mode">
          <ShieldCheck className="size-icon-xs shrink-0 opacity-70" />
          {modeLabel(permissions.mode)}
        </span>
      ) : null}

      {activeTools > 0 ? (
        <span className="tabular-nums" data-testid="header-active-tools">
          {activeTools} in flight
        </span>
      ) : null}

      {failures > 0 ? (
        <span className="text-error tabular-nums" data-testid="header-failures">
          {failures} failed
        </span>
      ) : null}
    </header>
  );
}
