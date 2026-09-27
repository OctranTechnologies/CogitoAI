import type { SessionSummary, WorkspaceSummary } from "./rpc";

export interface SidebarProject {
  path: string;
  label: string;
  /** null means the runtime has not told us whether this older workspace is a repository. */
  isRepository: boolean | null;
  active: boolean;
  sessions: SessionSummary[];
}

/** Deterministic v0 title: the first task, collapsed to one line and capped. */
export function makeSessionTitle(task: string): string {
  const normalized = task.trim().split(/\s+/).filter(Boolean).join(" ");
  if (!normalized) return "New session";
  const characters = Array.from(normalized);
  if (characters.length <= 64) return normalized;
  return `${characters.slice(0, 64).join("").trimEnd()}…`;
}

function normalizedPath(path: string): string {
  return path.replace(/\\/g, "/").replace(/\/+$/, "").toLocaleLowerCase();
}

function basename(path: string): string {
  return path.replace(/[\\/]+$/, "").split(/[\\/]/).filter(Boolean).at(-1) ?? path;
}

/** Groups at the strongest identity available: inspected repository root for
 * the open workspace and persisted workspace root for historical sessions. */
export function groupSessionsByWorkspace(
  sessions: SessionSummary[],
  workspacePath: string,
  workspace: WorkspaceSummary | null,
): SidebarProject[] {
  const openWorkspace = normalizedPath(workspacePath);
  const descriptionMatches =
    openWorkspace === normalizedPath(workspace?.current_directory ?? "");
  const repositoryRoot = descriptionMatches ? workspace?.repository_root ?? null : null;
  const activePath = repositoryRoot ?? workspacePath;
  const groups = new Map<string, SidebarProject>();

  for (const session of sessions) {
    const key = normalizedPath(session.workspace_root);
    let project = groups.get(key);
    if (!project) {
      const active = key === openWorkspace;
      const displayPath = active ? activePath : session.workspace_root;
      project = {
        path: session.workspace_root,
        label: basename(displayPath),
        isRepository: active ? Boolean(repositoryRoot) : null,
        active,
        sessions: [],
      };
      groups.set(key, project);
    }
    project.sessions.push(session);
  }

  if (workspacePath && !groups.has(openWorkspace)) {
    groups.set(openWorkspace, {
      path: workspacePath,
      label: basename(activePath),
      isRepository: Boolean(repositoryRoot),
      active: true,
      sessions: [],
    });
  }

  return [...groups.values()].sort((left, right) => {
    if (left.active !== right.active) return left.active ? -1 : 1;
    const leftRecent = left.sessions[0]?.last_updated_at ?? 0;
    const rightRecent = right.sessions[0]?.last_updated_at ?? 0;
    return rightRecent - leftRecent || left.label.localeCompare(right.label);
  });
}

export function sortSessionsByRecent(sessions: SessionSummary[]): SessionSummary[] {
  return [...sessions].sort(
    (left, right) =>
      right.last_updated_at - left.last_updated_at || right.created_at - left.created_at,
  );
}

export function filterSessions(sessions: SessionSummary[], query: string): SessionSummary[] {
  const needle = query.trim().toLocaleLowerCase();
  if (!needle) return sessions;
  return sessions.filter((session) =>
    `${session.title ?? ""} ${session.workspace_root}`.toLocaleLowerCase().includes(needle),
  );
}
