import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { open as openDialog } from "@tauri-apps/plugin-dialog";

vi.mock("@tauri-apps/plugin-dialog", () => ({ open: vi.fn(async () => null) }));
vi.mock("./components/terminal-panel", () => ({ TerminalPanel: () => null }));

const { default: App } = await import("./App");
const { useDesktopStore } = await import("./store");
const originalState = useDesktopStore.getState();

afterEach(() => {
  cleanup();
  useDesktopStore.setState(originalState);
});

beforeEach(() => {
  useDesktopStore.setState({
    ...originalState,
    status: "connected",
    clientId: null,
    workspacePath: "C:/work/repo",
    workspace: null,
    sessions: [],
    activeSessionId: null,
    runPhase: "idle",
    messages: [],
    events: [],
    composer: "fix the bug",
    isLoadingSession: false,
    terminal: null,
    isStartingTerminal: false,
    settings: null,
    createSession: vi.fn(),
    sendMessage: vi.fn(),
    startTerminal: vi.fn(),
    resumeSession: vi.fn(),
    connect: vi.fn(async () => undefined),
  });
});

describe("desktop keyboard workflow", () => {
  it("opens the palette from either shortcut and supports search, arrows, Enter, and Escape", () => {
    render(<App />);
    fireEvent.keyDown(window, { key: "p", ctrlKey: true, shiftKey: true });
    const input = screen.getByRole("textbox", { name: "Command palette" });
    expect(document.activeElement).toBe(input);

    fireEvent.change(input, { target: { value: "new task" } });
    fireEvent.keyDown(input, { key: "Enter" });
    expect(useDesktopStore.getState().createSession).toHaveBeenCalledOnce();
    expect(screen.queryByRole("dialog", { name: "Command palette" })).toBeNull();

    fireEvent.keyDown(window, { key: "k", metaKey: true });
    expect(screen.getByRole("dialog", { name: "Command palette" })).toBeTruthy();
    fireEvent.keyDown(document, { key: "Escape" });
    expect(screen.queryByRole("dialog", { name: "Command palette" })).toBeNull();
  });

  it("uses Ctrl/Cmd+P for session search and Ctrl/Cmd+N for a new task", () => {
    render(<App />);
    fireEvent.keyDown(window, { key: "p", ctrlKey: true });
    const search = screen.getByRole("searchbox", { name: "Filter sessions" });
    expect(document.activeElement).toBe(search);

    fireEvent.keyDown(window, { key: "n", metaKey: true });
    expect(useDesktopStore.getState().createSession).toHaveBeenCalledOnce();
  });

  it("submits the prompt once and starts the terminal from keyboard shortcuts", () => {
    render(<App />);
    const prompt = screen.getByRole("textbox", { name: "Message the agent" });
    fireEvent.keyDown(prompt, { key: "Enter", ctrlKey: true });
    expect(useDesktopStore.getState().sendMessage).toHaveBeenCalledOnce();
    expect(useDesktopStore.getState().sendMessage).toHaveBeenCalledWith("fix the bug");

    fireEvent.keyDown(window, { key: "`", code: "Backquote", ctrlKey: true });
    expect(useDesktopStore.getState().startTerminal).toHaveBeenCalledOnce();
    expect(screen.getByTestId("session-workspace")).toBeTruthy();
  });

  it("opens a selected project in the connected runtime", async () => {
    vi.mocked(openDialog).mockResolvedValue("D:/work/new-repo");
    render(<App />);
    fireEvent.keyDown(window, { key: "k", ctrlKey: true });
    const input = screen.getByRole("textbox", { name: "Command palette" });
    fireEvent.change(input, { target: { value: "open project" } });
    fireEvent.keyDown(input, { key: "Enter" });

    await waitFor(() => {
      expect(useDesktopStore.getState().connect).toHaveBeenCalledWith("auto", "D:/work/new-repo");
    });
    expect(useDesktopStore.getState().workspacePath).toBe("D:/work/new-repo");
  });

  it("keeps shortcuts out of an open dialog and Escape closes it", () => {
    render(<App />);
    fireEvent.keyDown(window, { key: "k", ctrlKey: true });
    const input = screen.getByRole("textbox", { name: "Command palette" });
    fireEvent.change(input, { target: { value: "open settings" } });
    fireEvent.keyDown(input, { key: "Enter" });
    expect(screen.getByRole("dialog", { name: "Settings" })).toBeTruthy();
    fireEvent.keyDown(window, { key: "n", ctrlKey: true });
    expect(useDesktopStore.getState().createSession).not.toHaveBeenCalled();
    fireEvent.keyDown(document, { key: "Escape" });
    expect(screen.queryByRole("dialog", { name: "Settings" })).toBeNull();
  });
});
