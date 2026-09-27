import { cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { ProjectSidebar } from "./project-sidebar";
import type { SessionSummary, WorkspaceSummary } from "../lib/rpc";
import { groupSessionsByWorkspace, sortSessionsByRecent } from "../lib/sidebar-model";

const roots = ["/work/Octran Website", "/work/Octran Nexus", "/work/Cogito Service", "/work/Docs"];

afterEach(cleanup);

function makeSessions(count: number): SessionSummary[] {
  return Array.from({ length: count }, (_, index) => {
    const workspace_root = roots[index % roots.length];
    return {
      id: `session-${index}`,
      workspace_root,
      title: index === 0 ? "Understand project structure" : `Task ${index}`,
      status: "Completed",
      created_at: index,
      last_updated_at: index,
      event_count: 4,
      context_compactions: 0,
    };
  });
}

const workspace: WorkspaceSummary = {
  current_directory: roots[0],
  repository_root: roots[0],
  languages: [],
  manifests: [],
  instructions: [],
  configuration: { package_manager: null, commands: {}, source: null },
};

function props(sessions: SessionSummary[], onSelectSession = vi.fn()) {
  const recent = sortSessionsByRecent(sessions);
  return {
    productName: "CogitoAI",
    sessions: recent,
    activeSessionId: null,
    workspacePath: roots[0],
    status: "connected",
    projects: groupSessionsByWorkspace(recent, roots[0], workspace),
    onSelectSession,
    onNewSession: vi.fn(),
    disabled: false,
  };
}

describe("project session sidebar", () => {
  it.each([1, 10, 50, 200])("groups and orders %i sessions by workspace", (count) => {
    const sessions = makeSessions(count);
    const started = performance.now();
    const projects = groupSessionsByWorkspace(sortSessionsByRecent(sessions), roots[0], workspace);
    const elapsed = performance.now() - started;

    expect(projects.reduce((total, project) => total + project.sessions.length, 0)).toBe(count);
    expect(projects[0].path).toBe(roots[0]);
    expect(projects[0].sessions[0].last_updated_at).toBeGreaterThanOrEqual(
      projects[0].sessions.at(-1)?.last_updated_at ?? 0,
    );
    expect(elapsed).toBeLessThan(100);
  });

  it("keeps a 200-session sidebar responsive, scrollable, and preview-sized", () => {
    const started = performance.now();
    const { container } = render(<ProjectSidebar {...props(makeSessions(200))} />);
    const elapsed = performance.now() - started;

    expect(elapsed).toBeLessThan(1000);
    expect(container.querySelector(".scroll-area")?.className).toContain("min-h-0 flex-1");
    expect(screen.getAllByTestId("sidebar-session").length).toBeLessThanOrEqual(RECENT_PREVIEW + PROJECT_PREVIEW);
    expect(screen.getByRole("button", { name: "Show all" })).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Show all" }));
    expect(screen.getAllByTestId("sidebar-session").length).toBeGreaterThanOrEqual(200);
  });

  it("expands project tasks and keeps the human-readable task title primary", () => {
    render(<ProjectSidebar {...props(makeSessions(10))} />);
    expect(screen.getByText("Projects")).toBeTruthy();
    expect(screen.getByText("Recents")).toBeTruthy();
    expect(screen.getByText("Understand project structure")).toBeTruthy();

    const projectToggle = screen.getByRole("button", { name: "Expand Octran Nexus" });
    fireEvent.click(projectToggle);
    const project = projectToggle.closest("li[data-testid='sidebar-project']");
    expect(project instanceof HTMLElement && within(project).getByText("Task 1")).toBeTruthy();
    expect(project?.querySelector("svg[aria-label='Workspace']")).toBeTruthy();
  });

  it("filters task names, resumes a selected session, and preserves selected state", () => {
    const onSelectSession = vi.fn();
    render(<ProjectSidebar {...props(makeSessions(200), onSelectSession)} />);
    fireEvent.click(screen.getByRole("button", { name: "Search sessions" }));
    fireEvent.change(screen.getByRole("searchbox", { name: "Filter sessions" }), {
      target: { value: "Task 199" },
    });

    const matches = screen.getAllByTestId("sidebar-session");
    expect(matches).toHaveLength(2);
    expect(matches[0].textContent).toContain("Task 199");
    fireEvent.click(matches[0]);
    expect(onSelectSession).toHaveBeenCalledWith("session-199");
    fireEvent.change(screen.getByRole("searchbox", { name: "Filter sessions" }), {
      target: { value: "Octran Nexus" },
    });
    expect(screen.getAllByTestId("sidebar-session")).toHaveLength(100);

    const selected = render(<ProjectSidebar {...props(makeSessions(10), onSelectSession)} activeSessionId="session-9" />);
    expect(selected.getAllByTestId("sidebar-session").some((button) => button.getAttribute("aria-current") === "true")).toBe(true);
  });

  it("provides project and session context actions", () => {
    render(<ProjectSidebar {...props(makeSessions(10))} />);
    fireEvent.contextMenu(screen.getByRole("button", { name: "Collapse Octran Website" }));
    expect(screen.getByRole("menuitem", { name: "Collapse project" })).toBeTruthy();
    fireEvent.keyDown(document, { key: "Escape" });

    fireEvent.contextMenu(screen.getAllByTestId("sidebar-session")[0]);
    expect(screen.getByRole("menuitem", { name: "Resume session" })).toBeTruthy();
  });

  it("moves keyboard focus through project and session navigation", () => {
    render(<ProjectSidebar {...props(makeSessions(10))} />);
    const projectToggle = screen.getByRole("button", { name: "Collapse Octran Website" });
    projectToggle.focus();
    fireEvent.keyDown(projectToggle, { key: "ArrowDown" });
    expect(document.activeElement?.getAttribute("data-session-id")).toBe("session-8");
  });
});

const RECENT_PREVIEW = 8;
const PROJECT_PREVIEW = 3;
