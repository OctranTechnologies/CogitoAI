import { useEffect, useMemo, useRef, useState } from "react";
import {
  ChevronDown,
  ChevronRight,
  Clock3,
  Folder,
  FolderGit2,
  GitBranch,
  MessageSquare,
  MoreHorizontal,
  Play,
  Search,
  X,
} from "lucide-react";
import type { SessionSummary } from "../lib/rpc";
import { filterSessions, type SidebarProject } from "../lib/sidebar-model";
import { Button, ContextMenu, Dropdown, ScrollArea, Separator, StatusIndicator, Tooltip, cx } from "./ui";

const RECENT_PREVIEW = 8;
const PROJECT_PREVIEW = 3;

export interface ProjectSidebarProps {
  productName: string;
  sessions: SessionSummary[];
  activeSessionId: string | null;
  workspacePath: string;
  status: string;
  projects: SidebarProject[];
  onSelectSession: (id: string) => void;
  onNewSession: () => void;
  disabled: boolean;
}

/** Project-scoped navigation for durable coding tasks. */
export function ProjectSidebar({
  productName,
  sessions,
  activeSessionId,
  workspacePath,
  status,
  projects,
  onSelectSession,
  onNewSession,
  disabled,
}: ProjectSidebarProps) {
  const [searchOpen, setSearchOpen] = useState(false);
  const [query, setQuery] = useState("");
  const [expanded, setExpanded] = useState<Record<string, boolean>>({});
  const [showAllRecent, setShowAllRecent] = useState(false);
  const searchRef = useRef<HTMLInputElement>(null);
  const searching = query.trim().length > 0;
  const filteredSessions = useMemo(
    () => filterSessions(sessions, query),
    [sessions, query],
  );
  const filteredIds = useMemo(
    () => new Set(filteredSessions.map((session) => session.id)),
    [filteredSessions],
  );
  const recents = searching ? filteredSessions : sessions;
  const visibleRecents = searching || showAllRecent ? recents : recents.slice(0, RECENT_PREVIEW);

  useEffect(() => {
    if (searchOpen) searchRef.current?.focus();
  }, [searchOpen]);

  function isExpanded(project: SidebarProject): boolean {
    return expanded[project.path] ?? project.active;
  }

  function toggleProject(path: string) {
    const project = projects.find((entry) => entry.path === path);
    setExpanded((current) => ({ ...current, [path]: !(current[path] ?? project?.active ?? false) }));
  }

  function handleNavigationKey(event: React.KeyboardEvent<HTMLDivElement>) {
    if (event.target instanceof HTMLElement && event.target.closest('[role="menu"]')) return;
    if (!["ArrowDown", "ArrowUp", "Home", "End"].includes(event.key)) return;
    if (!(event.target instanceof HTMLElement) || !event.target.matches("[data-sidebar-nav-item='true']")) return;
    const items = Array.from(
      event.currentTarget.querySelectorAll<HTMLButtonElement>("[data-sidebar-nav-item='true']:not(:disabled)"),
    );
    if (items.length === 0) return;
    const current = items.indexOf(event.target as HTMLButtonElement);
    const next = event.key === "Home"
      ? 0
      : event.key === "End"
        ? items.length - 1
        : (current + (event.key === "ArrowDown" ? 1 : -1) + items.length) % items.length;
    event.preventDefault();
    items[next]?.focus();
  }

  return (
    <aside className="flex w-[296px] shrink-0 flex-col border-r border-line bg-panel" aria-label="Projects and sessions">
      <header className="flex h-12 shrink-0 items-center gap-1.5 px-3">
        <span className="truncate text-md font-semibold tracking-tight text-primary">{productName}</span>
        <Tooltip label={workspacePath || "No workspace open"}>
          <span className="flex size-6 shrink-0 items-center justify-center text-muted" aria-label="Current workspace">
            <Folder className="size-icon-sm" />
          </span>
        </Tooltip>
        <div className="ml-auto flex items-center gap-0.5">
          <Tooltip label={searchOpen ? "Close session search" : "Search sessions"}>
            <button
              type="button"
              aria-label={searchOpen ? "Close session search" : "Search sessions"}
              aria-expanded={searchOpen}
              onClick={() => {
                setSearchOpen((open) => !open);
                setQuery("");
              }}
              className="flex size-7 items-center justify-center rounded-md text-muted transition-colors duration-fast hover:bg-hover hover:text-primary"
            >
              {searchOpen ? <X className="size-icon-sm" /> : <Search className="size-icon-md" />}
            </button>
          </Tooltip>
          <Tooltip label="Runtime status">
            <span className="flex h-7 items-center px-1.5"><StatusIndicator status={status} label={false} /></span>
          </Tooltip>
        </div>
      </header>

      <div className="px-3 pb-2">
        <Button variant="primary" block onClick={onNewSession} disabled={disabled} icon={<Play className="size-icon-sm" />}>
          New task
        </Button>
      </div>

      {searchOpen ? (
        <div className="px-3 pb-2">
          <label className="sr-only" htmlFor="sidebar-session-search">Filter sessions</label>
          <div className="flex h-8 items-center gap-2 rounded-md border border-line bg-app px-2 text-muted focus-within:border-line-strong">
            <Search className="size-icon-sm shrink-0" aria-hidden="true" />
            <input
              ref={searchRef}
              id="sidebar-session-search"
              type="search"
              value={query}
              onChange={(event) => setQuery(event.target.value)}
              onKeyDown={(event) => {
                if (event.key === "Escape") {
                  if (query) setQuery("");
                  else setSearchOpen(false);
                }
              }}
              placeholder="Filter tasks and projects"
              className="min-w-0 flex-1 bg-transparent text-xs text-primary outline-none placeholder:text-faint"
              autoComplete="off"
              spellCheck={false}
            />
            {query ? <span className="font-mono text-2xs tabular-nums text-faint">{filteredSessions.length}</span> : null}
          </div>
        </div>
      ) : null}

      <ScrollArea className="min-h-0 flex-1 px-2 pb-3" onKeyDown={handleNavigationKey}>
        <section className="mb-3" aria-labelledby="sidebar-projects-heading">
          <p id="sidebar-projects-heading" className="label-mono px-2 py-1.5">Projects</p>
          {projects.length === 0 ? (
            <p className="px-2 py-1.5 text-2xs leading-4 text-faint">Connect a runtime to open a project.</p>
          ) : (
            <ul className="space-y-0.5">
              {projects.map((project) => {
                const expandedNow = isExpanded(project) || searching;
                const projectMatches = !searching || `${project.label} ${project.path}`.toLocaleLowerCase().includes(query.trim().toLocaleLowerCase());
                const visibleSessions = projectMatches && searching
                  ? project.sessions
                  : project.sessions.filter((session) => filteredIds.has(session.id));
                const projectVisible = projectMatches || visibleSessions.length > 0;
                if (!projectVisible) return null;
                const shownSessions = searching || expanded[project.path] === true
                  ? visibleSessions
                  : visibleSessions.slice(0, PROJECT_PREVIEW);
                const moreCount = visibleSessions.length - shownSessions.length;
                const ProjectIcon = project.isRepository ? FolderGit2 : Folder;
                const menuItems = [
                  {
                    id: "toggle-project",
                    label: expandedNow ? "Collapse project" : "Expand project",
                    icon: expandedNow ? <ChevronRight className="size-icon-sm" /> : <ChevronDown className="size-icon-sm" />,
                    onSelect: () => toggleProject(project.path),
                  },
                ];
                return (
                  <li key={project.path} data-testid="sidebar-project" data-project-path={project.path}>
                    <ContextMenu label={`Project actions for ${project.label}`} items={menuItems}>
                    <div>
                    <div className="group flex min-w-0 items-center">
                      <button
                        type="button"
                        data-sidebar-nav-item="true"
                        data-testid="sidebar-project-toggle"
                        aria-expanded={expandedNow}
                        aria-label={`${expandedNow ? "Collapse" : "Expand"} ${project.label}`}
                        title={project.path}
                        onClick={() => toggleProject(project.path)}
                        className={cx(
                          "flex min-w-0 flex-1 items-center gap-2 rounded-md px-2 py-1.5 text-left transition-colors duration-fast",
                          project.active ? "text-primary" : "text-secondary hover:bg-hover hover:text-primary",
                        )}
                      >
                        {expandedNow ? <ChevronDown className="size-icon-xs shrink-0 text-faint" /> : <ChevronRight className="size-icon-xs shrink-0 text-faint" />}
                        <ProjectIcon className={cx("size-icon-sm shrink-0", project.active ? "text-accent" : "text-muted")} />
                        <span className="min-w-0 flex-1 truncate text-xs" title={project.label}>{project.label}</span>
                        {project.isRepository === true ? (
                          <Tooltip label={`Git repository · ${project.path}`}>
                            <GitBranch className="size-icon-xs shrink-0 text-faint" aria-label="Git repository" />
                          </Tooltip>
                        ) : (
                          <Tooltip label={`Workspace · ${project.path}`}>
                            <Folder className="size-icon-xs shrink-0 text-faint" aria-label="Workspace" />
                          </Tooltip>
                        )}
                        <span className="shrink-0 font-mono text-2xs tabular-nums text-faint">{project.sessions.length}</span>
                      </button>
                      <Dropdown
                        label={`More actions for ${project.label}`}
                        placement="bottom-end"
                        className="mr-1 opacity-0 transition-opacity duration-fast group-hover:opacity-100 focus-within:opacity-100"
                        trigger={<MoreHorizontal className="size-icon-sm text-faint" />}
                        items={menuItems}
                      />
                    </div>
                    {expandedNow && shownSessions.length > 0 ? (
                      <ul className="ml-[18px] mt-0.5 space-y-px border-l border-line pl-2">
                        {shownSessions.map((session) => (
                          <li key={session.id}>
                            <SessionRow session={session} selected={session.id === activeSessionId} onResume={onSelectSession} />
                          </li>
                        ))}
                        {moreCount > 0 ? (
                          <li>
                            <button
                              type="button"
                              onClick={() => setExpanded((current) => ({ ...current, [project.path]: true }))}
                              className="w-full rounded-md px-2 py-1 text-left text-2xs text-muted hover:bg-hover hover:text-primary"
                            >
                              Show {moreCount} more tasks
                            </button>
                          </li>
                        ) : null}
                      </ul>
                    ) : null}
                    {expandedNow && shownSessions.length === 0 && searching && projectMatches ? (
                      <p className="ml-7 px-2 py-1 text-2xs text-faint">No matching tasks in this project.</p>
                    ) : null}
                    </div>
                    </ContextMenu>
                  </li>
                );
              })}
            </ul>
          )}
        </section>

        <section aria-labelledby="sidebar-recents-heading">
          <div className="flex items-center justify-between px-2 py-1.5">
            <p id="sidebar-recents-heading" className="label-mono">Recents</p>
            {!searching && sessions.length > RECENT_PREVIEW ? (
              <button
                type="button"
                onClick={() => setShowAllRecent((value) => !value)}
                className="rounded px-1 text-2xs text-muted transition-colors duration-fast hover:text-primary focus-visible:text-primary"
              >
                {showAllRecent ? "Show less" : "Show all"}
              </button>
            ) : null}
          </div>
          {visibleRecents.length === 0 ? (
            <p className="px-2 py-1.5 text-2xs leading-4 text-faint">
              {sessions.length === 0 ? "No sessions yet." : "No matching sessions."}
            </p>
          ) : (
            <ul className="space-y-px">
              {visibleRecents.map((session) => (
                <li key={session.id}>
                  <SessionRow session={session} selected={session.id === activeSessionId} onResume={onSelectSession} recent />
                </li>
              ))}
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

function SessionRow({
  session,
  selected,
  onResume,
  recent = false,
}: {
  session: SessionSummary;
  selected: boolean;
  onResume: (id: string) => void;
  recent?: boolean;
}) {
  const title = session.title?.trim() || (session.event_count > 1 ? "Untitled session" : "New session");
  const item = (
    <div className="group flex min-w-0 items-center rounded-md pr-0.5">
      <button
        type="button"
        data-sidebar-nav-item="true"
        data-testid="sidebar-session"
        data-session-id={session.id}
        aria-current={selected ? "true" : undefined}
        title={title}
        onClick={() => onResume(session.id)}
        className={cx(
          "flex min-w-0 flex-1 items-center gap-2 rounded-md px-2 py-1.5 text-left transition-colors duration-fast",
          selected ? "bg-active text-primary" : "text-secondary hover:bg-hover hover:text-primary",
        )}
      >
        {recent ? <Clock3 className="size-icon-xs shrink-0 text-faint" /> : <MessageSquare className="size-icon-xs shrink-0 text-faint" />}
        <Tooltip className="min-w-0 flex-1" label={title}>
          <span className="block min-w-0 flex-1 truncate text-xs leading-4">{title}</span>
        </Tooltip>
      </button>
      <Dropdown
        label={`More actions for ${title}`}
        placement="bottom-end"
        className="opacity-0 transition-opacity duration-fast group-hover:opacity-100 focus-within:opacity-100"
        trigger={<MoreHorizontal className="size-icon-sm text-faint" />}
        items={[
          {
            id: "resume-session",
            label: "Resume session",
            icon: <Play className="size-icon-sm" />,
            onSelect: () => onResume(session.id),
          },
        ]}
      />
    </div>
  );
  return (
    <ContextMenu
      label={`Session actions for ${title}`}
      items={[{
        id: "resume-session",
        label: "Resume session",
        icon: <Play className="size-icon-sm" />,
        onSelect: () => onResume(session.id),
      }]}
    >
      {item}
    </ContextMenu>
  );
}
