import { useState } from "react";
import {
  Check,
  ChevronDown,
  FolderGit2,
  MessageSquare,
  Play,
  Search,
  Timer,
} from "lucide-react";
import type { SessionSummary } from "../lib/rpc";
import { Button, ScrollArea, Separator, StatusIndicator, Tooltip, cx, toneText } from "./ui";
import { toneFromStatus } from "./ui/tone";

export interface ProjectEntry {
  path: string;
  label: string;
  sessionCount: number;
  active: boolean;
}

export interface ProjectSidebarProps {
  productName: string;
  sessions: SessionSummary[];
  activeSessionId: string | null;
  workspacePath: string;
  status: string;
  /** Named workspaces derived from the sessions the runtime reports. */
  projects: ProjectEntry[];
  onSelectSession: (id: string) => void;
  onNewSession: () => void;
  onSelectProject: (path: string) => void;
  disabled: boolean;
  onSearch: () => void;
}

/**
 * Project and session sidebar.
 *
 * The product identity and the workspace switcher live here rather than in a
 * top bar, because in a three-region layout the top-left corner belongs to the
 * region that scopes everything else. The body scrolls independently of the
 * header so a long session list never pushes the new-session action off screen.
 */
export function ProjectSidebar({
  productName,
  sessions,
  activeSessionId,
  workspacePath,
  status,
  projects,
  onSelectSession,
  onNewSession,
  onSelectProject,
  disabled,
  onSearch,
}: ProjectSidebarProps) {
  const [showAll, setShowAll] = useState(false);
  const visible = showAll ? sessions : sessions.slice(0, 12);

  return (
    <aside className="flex w-[296px] shrink-0 flex-col border-r border-line bg-panel">
      <header className="flex h-12 shrink-0 items-center gap-1.5 px-3">
        <span className="truncate text-md font-semibold tracking-tight text-primary">
          {productName}
        </span>
        <Tooltip label={workspacePath || "No workspace open"}>
          <button
            type="button"
            aria-label="Switch workspace"
            className="flex size-6 shrink-0 items-center justify-center rounded-md text-muted transition-colors duration-fast hover:bg-hover hover:text-primary"
            onClick={() => onSelectProject(workspacePath)}
          >
            <ChevronDown className="size-icon-sm" />
          </button>
        </Tooltip>
        <div className="ml-auto flex items-center gap-0.5">
          <Tooltip label="Search sessions">
            <button
              type="button"
              aria-label="Search sessions"
              onClick={onSearch}
              className="flex size-7 items-center justify-center rounded-md text-muted transition-colors duration-fast hover:bg-hover hover:text-primary"
            >
              <Search className="size-icon-md" />
            </button>
          </Tooltip>
          <Tooltip label="Runtime status">
            <span className="flex h-7 items-center px-1.5">
              <StatusIndicator status={status} label={false} />
            </span>
          </Tooltip>
        </div>
      </header>

      <div className="px-3 pb-2">
        <Button
          variant="primary"
          block
          onClick={onNewSession}
          disabled={disabled}
          icon={<Play className="size-icon-sm" />}
        >
          New task
        </Button>
      </div>

      <ScrollArea className="min-h-0 flex-1 px-2 pb-3">
        <section className="mb-3">
          <p className="label-mono px-2 py-1.5">Projects</p>
          {projects.length === 0 ? (
            <p className="px-2 py-1.5 text-2xs leading-4 text-faint">
              Connect a runtime to open a project.
            </p>
          ) : (
            <ul className="space-y-0.5">
              {projects.map((project) => (
                <li key={project.path}>
                  <button
                    type="button"
                    onClick={() => onSelectProject(project.path)}
                    aria-current={project.active ? "true" : undefined}
                    className={cx(
                      "flex w-full items-center gap-2 rounded-md px-2 py-1.5 text-left",
                      "transition-colors duration-fast",
                      project.active
                        ? "bg-active text-primary"
                        : "text-muted hover:bg-hover hover:text-primary",
                    )}
                  >
                    <FolderGit2 className="size-icon-sm shrink-0 text-faint" />
                    <span className="min-w-0 flex-1">
                      <span className="block truncate text-xs">{project.label}</span>
                      <span className="block truncate font-mono text-2xs text-faint">
                        {project.sessionCount} session{project.sessionCount === 1 ? "" : "s"}
                      </span>
                    </span>
                    {project.active ? (
                      <Check className="size-icon-sm shrink-0 text-accent" />
                    ) : null}
                  </button>
                </li>
              ))}
            </ul>
          )}
        </section>

        <section>
          <div className="flex items-center justify-between px-2 py-1.5">
            <p className="label-mono">Recents</p>
            {sessions.length > 12 ? (
              <button
                type="button"
                onClick={() => setShowAll((value) => !value)}
                className="text-2xs text-muted transition-colors duration-fast hover:text-primary"
              >
                {showAll ? "Show less" : `Show all ${sessions.length}`}
              </button>
            ) : null}
          </div>
          {visible.length === 0 ? (
            <p className="px-2 py-1.5 text-2xs leading-4 text-faint">
              {sessions.length === 0 ? "No sessions yet." : "No sessions to show."}
            </p>
          ) : (
            <ul className="space-y-0.5">
              {visible.map((session) => {
                const selected = session.id === activeSessionId;
                return (
                  <li key={session.id}>
                    <button
                      type="button"
                      onClick={() => onSelectSession(session.id)}
                      aria-current={selected ? "true" : undefined}
                      className={cx(
                        "flex w-full items-start gap-2 rounded-md px-2 py-1.5 text-left",
                        "transition-colors duration-fast",
                        selected
                          ? "bg-active text-primary"
                          : "text-muted hover:bg-hover hover:text-primary",
                      )}
                    >
                      {session.status === "Completed" ? (
                        <Timer className="mt-0.5 size-icon-sm shrink-0 text-faint" />
                      ) : (
                        <MessageSquare className="mt-0.5 size-icon-sm shrink-0 text-faint" />
                      )}
                      <span className="min-w-0 flex-1">
                        <span className="block truncate font-mono text-2xs">{session.id}</span>
                        <span className="mt-0.5 flex items-center gap-1.5 text-2xs text-faint">
                          <span className={toneText(toneFromStatus(session.status))}>
                            {session.status}
                          </span>
                          <span>{session.event_count} events</span>
                        </span>
                      </span>
                    </button>
                  </li>
                );
              })}
            </ul>
          )}
        </section>
      </ScrollArea>

      <Separator />
      <footer className="shrink-0 px-3 py-2">
        <p className="truncate font-mono text-2xs text-faint" title={workspacePath}>
          {workspacePath || "no workspace"}
        </p>
      </footer>
    </aside>
  );
}
