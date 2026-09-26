import {
  CheckCircle2,
  CircleDot,
  Clock3,
  FolderGit2,
  ShieldCheck,
  Wrench,
  XCircle,
} from "lucide-react";
import type { ToolActivity, TimelineEntry, VerificationActivity } from "../store";
import type { ProjectSettings } from "../lib/settings";
import { Badge, EmptyState, ScrollArea, cx, toneText, type Tone } from "./ui";
import { toneFromStatus } from "./ui/tone";

/**
 * Region bodies for the rail destinations that are not the conversation.
 *
 * Every value shown here comes from the runtime: the session timeline, the tool
 * and verification streams, and the project settings snapshot. Nothing is
 * synthesised, so an empty region is genuinely empty rather than filled with
 * placeholders.
 */

export function HistoryView({
  timeline,
  sessions,
  onSelectSession,
  activeSessionId,
}: {
  timeline: TimelineEntry[];
  sessions: { id: string; status: string; event_count: number; created_at: number }[];
  onSelectSession: (id: string) => void;
  activeSessionId: string | null;
}) {
  return (
    <div className="flex min-h-0 flex-1">
      <div className="flex w-64 shrink-0 flex-col border-r border-line">
        <header className="flex h-10 shrink-0 items-center border-b border-line px-3">
          <p className="label-mono">All sessions</p>
        </header>
        <ScrollArea className="flex-1 p-2">
          {sessions.length === 0 ? (
            <EmptyState>No sessions reported by the runtime.</EmptyState>
          ) : (
            <ul className="space-y-0.5">
              {sessions.map((session) => (
                <li key={session.id}>
                  <button
                    type="button"
                    onClick={() => onSelectSession(session.id)}
                    aria-current={session.id === activeSessionId ? "true" : undefined}
                    className={cx(
                      "flex w-full flex-col items-start gap-0.5 rounded-md px-2 py-1.5 text-left",
                      "transition-colors duration-fast",
                      session.id === activeSessionId
                        ? "bg-active text-primary"
                        : "text-muted hover:bg-hover hover:text-primary",
                    )}
                  >
                    <span className="truncate font-mono text-2xs">{session.id}</span>
                    <span className="text-2xs text-faint">
                      <span className={toneText(toneFromStatus(session.status))}>
                        {session.status}
                      </span>{" "}
                      · {session.event_count} events
                    </span>
                  </button>
                </li>
              ))}
            </ul>
          )}
        </ScrollArea>
      </div>

      <div className="flex min-w-0 flex-1 flex-col">
        <header className="flex h-10 shrink-0 items-center border-b border-line px-3">
          <p className="label-mono">Session timeline</p>
          <span className="ml-2 text-2xs text-faint">{timeline.length} events</span>
        </header>
        <ScrollArea className="flex-1 p-3">
          {timeline.length === 0 ? (
            <EmptyState>
              The runtime streams durable events here as a run progresses. Nothing has been recorded
              for this session yet.
            </EmptyState>
          ) : (
            <ol className="space-y-0.5">
              {timeline.map((entry) => (
                <li key={entry.id}>
                  <div className="grid grid-cols-[16px_1fr] gap-2 rounded-md px-2 py-1.5 text-2xs leading-4 hover:bg-hover">
                    <span className={cx("mt-0.5", toneText(eventTone(entry.tone)))}>
                      {entry.tone === "danger" ? (
                        <XCircle className="size-icon-sm" />
                      ) : entry.tone === "success" ? (
                        <CheckCircle2 className="size-icon-sm" />
                      ) : (
                        <CircleDot className="size-icon-sm" />
                      )}
                    </span>
                    <span className="min-w-0">
                      <span className="block truncate font-mono text-secondary">
                        {entry.eventType}
                      </span>
                      {entry.detail ? (
                        <span className="block truncate text-faint">{entry.detail}</span>
                      ) : null}
                    </span>
                  </div>
                </li>
              ))}
            </ol>
          )}
        </ScrollArea>
      </div>
    </div>
  );
}

export function ActivityView({
  tools,
  verification,
}: {
  tools: ToolActivity[];
  verification: VerificationActivity[];
}) {
  return (
    <ScrollArea className="min-h-0 flex-1 p-4">
      <div className="mx-auto max-w-3xl space-y-5">
        <section>
          <p className="label-mono mb-2">Tool activity</p>
          {tools.length === 0 ? (
            <EmptyState>Tool calls appear here once the runtime reports them.</EmptyState>
          ) : (
            <ul className="space-y-1">
              {[...tools].reverse().map((tool) => (
                <li
                  key={tool.id}
                  className="flex items-center gap-2 rounded-md border border-line bg-elevated px-2.5 py-2 text-2xs"
                >
                  <Wrench
                    className={cx("size-icon-sm shrink-0", toneText(toolTone(tool.state)))}
                  />
                  <span className="shrink-0 text-secondary">{tool.name}</span>
                  <span className="min-w-0 flex-1 truncate font-mono text-faint">
                    {tool.target || "no target"}
                  </span>
                  {tool.durationMs !== null ? (
                    <span className="flex shrink-0 items-center gap-1 text-faint">
                      <Clock3 className="size-icon-xs" />
                      {formatDuration(tool.durationMs)}
                    </span>
                  ) : null}
                  <Badge tone={toolTone(tool.state)}>{tool.state}</Badge>
                </li>
              ))}
            </ul>
          )}
        </section>

        <section>
          <p className="label-mono mb-2">Verification</p>
          {verification.length === 0 ? (
            <EmptyState>Verification results appear here after the agent edits files.</EmptyState>
          ) : (
            <ul className="space-y-1">
              {[...verification].reverse().map((item) => (
                <li
                  key={item.id}
                  className="flex items-center gap-2 rounded-md border border-line bg-elevated px-2.5 py-2 text-2xs"
                >
                  {item.state === "passed" ? (
                    <CheckCircle2 className={cx("size-icon-sm", toneText("success"))} />
                  ) : (
                    <CircleDot
                      className={cx("size-icon-sm", toneText(verificationTone(item.state)), item.state === "running" && "animate-pulse")}
                    />
                  )}
                  <span className="shrink-0 text-secondary">{item.category}</span>
                  <span className="min-w-0 flex-1 truncate font-mono text-faint">{item.command}</span>
                  {item.durationMs !== null ? (
                    <span className="shrink-0 text-faint">{formatDuration(item.durationMs)}</span>
                  ) : null}
                  <Badge tone={verificationTone(item.state)}>{item.state}</Badge>
                </li>
              ))}
            </ul>
          )}
        </section>

        <section>
          <p className="label-mono mb-2">Approvals</p>
          <EmptyState>
            Approvals are resolved inline in the conversation. A declined or allowed call stays in the
            session transcript.
          </EmptyState>
        </section>
      </div>
    </ScrollArea>
  );
}

export function ProjectsView({ project }: { project: ProjectSettings | null }) {
  if (!project) {
    return (
      <ScrollArea className="min-h-0 flex-1 p-4">
        <EmptyState>
          Project details come from the runtime workspace inspection. Connect a runtime to read
          them.
        </EmptyState>
      </ScrollArea>
    );
  }
  return (
    <ScrollArea className="min-h-0 flex-1 p-4">
      <div className="mx-auto max-w-3xl space-y-4">
        <section className="rounded-lg border border-line bg-panel p-3">
          <div className="flex items-center gap-2">
            <FolderGit2 className="size-icon-md text-accent" />
            <span className="text-md font-semibold text-primary">Open project</span>
            <Badge tone={project.is_git_repository ? "success" : "neutral"}>
              {project.is_git_repository ? "git" : "no git"}
            </Badge>
            {project.monorepo ? <Badge tone="accent">monorepo</Badge> : null}
          </div>
          <dl className="mt-3 grid grid-cols-[10rem_1fr] gap-y-2 text-xs">
            <dt className="text-faint">Workspace</dt>
            <dd className="break-all font-mono text-secondary">{project.workspace_path}</dd>
            <dt className="text-faint">Repository</dt>
            <dd className="break-all font-mono text-secondary">
              {project.repository_root ?? "not a Git repository"}
            </dd>
            <dt className="text-faint">Package manager</dt>
            <dd className="font-mono text-secondary">{project.package_manager ?? "none detected"}</dd>
          </dl>
        </section>

        <ChipSection title="Languages" items={project.languages} empty="No languages detected." />
        <ChipSection title="Manifests" items={project.manifests} empty="No manifests detected." />
        <ChipSection
          title="Instruction files"
          items={project.instruction_files}
          empty="No instruction files detected."
        />

        <section>
          <p className="label-mono mb-2">Policy</p>
          <p className="flex items-start gap-2 rounded-md border border-line bg-panel p-2.5 text-xs leading-4 text-muted">
            <ShieldCheck className="mt-0.5 size-icon-sm shrink-0 text-accent" />
            The active execution mode and its rules are in Settings › Permissions. Human terminal
            sessions are separate and are not governed by agent policy.
          </p>
        </section>
      </div>
    </ScrollArea>
  );
}

function ChipSection({ title, items, empty }: { title: string; items: string[]; empty: string }) {
  return (
    <section>
      <p className="label-mono mb-2">{title}</p>
      {items.length === 0 ? (
        <EmptyState>{empty}</EmptyState>
      ) : (
        <div className="flex flex-wrap gap-1">
          {items.map((item) => (
            <span
              key={item}
              title={item}
              className="max-w-full truncate rounded-sm bg-elevated px-1.5 py-0.5 font-mono text-2xs text-secondary"
            >
              {item}
            </span>
          ))}
        </div>
      )}
    </section>
  );
}

function eventTone(tone: TimelineEntry["tone"]): Tone {
  if (tone === "success") return "success";
  if (tone === "danger") return "error";
  if (tone === "warning") return "warning";
  return "accent";
}

function toolTone(state: ToolActivity["state"]): Tone {
  if (state === "succeeded") return "success";
  if (state === "failed" || state === "denied") return "error";
  if (state === "running") return "accent";
  if (state === "awaiting_approval") return "warning";
  return "neutral";
}

function verificationTone(state: VerificationActivity["state"]): Tone {
  if (state === "passed") return "success";
  if (state === "failed") return "error";
  return "accent";
}

function formatDuration(durationMs: number): string {
  if (durationMs < 1000) return `${durationMs}ms`;
  return `${(durationMs / 1000).toFixed(1)}s`;
}
