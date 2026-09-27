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
    <header className="flex h-9 min-w-0 flex-1 shrink-0 flex-nowrap items-center gap-x-3 overflow-hidden border-b border-line px-4 text-2xs text-faint">
      {projectName ? (
        <span className="min-w-0 max-w-[35%] shrink truncate font-medium text-secondary" title={projectName} data-testid="header-project">
          {projectName}
        </span>
      ) : null}

      {branch ? (
        <span className="inline-flex min-w-0 max-w-[22%] shrink items-center gap-1" title={branch} data-testid="header-branch">
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
        <span
          className="hidden min-w-0 max-w-[240px] shrink items-center gap-1 2xl:inline-flex"
          title={`${models.provider}/${models.model}`}
          data-testid="header-model"
        >
          <Cpu className="size-icon-xs shrink-0 opacity-70" />
          <span className="truncate">
            {models.provider}/{models.model}
          </span>
        </span>
      ) : null}

      {permissions ? (
        <span className="inline-flex shrink-0 items-center gap-1 whitespace-nowrap" data-testid="header-mode">
          <ShieldCheck className="size-icon-xs shrink-0 opacity-70" />
          {modeLabel(permissions.mode)}
        </span>
      ) : null}

      {activeTools > 0 ? (
        <span className="shrink-0 whitespace-nowrap tabular-nums" data-testid="header-active-tools">
          {activeTools} in flight
        </span>
      ) : null}

      {failures > 0 ? (
        <span className="shrink-0 whitespace-nowrap text-error tabular-nums" data-testid="header-failures">
          {failures} failed
        </span>
      ) : null}
    </header>
  );
}
