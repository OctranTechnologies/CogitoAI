import { beforeEach, describe, expect, it, vi } from "vitest";
import { useDesktopStore, subscribeTerminalExit, subscribeTerminalOutput } from "./store";
import { describeExit, isPtyInfo } from "./lib/terminal";
import type { RpcResponse, ServerMessage } from "./lib/rpc";

const mocks = vi.hoisted(() => ({
  connectRuntime: vi.fn(),
  disconnectRuntime: vi.fn(),
  requestRuntime: vi.fn(),
  receiveRuntimeMessage: vi.fn(),
}));

const { connectRuntime, requestRuntime } = mocks;

vi.mock("./lib/rpc", async (importOriginal) => {
  const actual = await importOriginal<typeof import("./lib/rpc")>();
  return { ...actual, ...mocks };
});

function response<T>(result: T): RpcResponse<T> {
  return { version: 1, id: "request-1", ok: true, result };
}

function notification(method: string, params: Record<string, unknown>): ServerMessage {
  return { kind: "notification", notification: { version: 1, method, params } };
}

const TERMINAL = {
  id: "pty-1",
  program: "cmd.exe",
  working_directory: "/repo",
  origin: "human",
  cols: 80,
  rows: 24,
  pid: 4242,
};

function resetStore() {
  useDesktopStore.setState({
    status: "unavailable",
    address: "127.0.0.1:4545",
    clientId: null,
    workspacePath: "",
    workspace: null,
    sessions: [],
    activeSessionId: null,
    activeRunId: null,
    runPhase: "idle",
    messages: [],
    events: [],
    toolActivity: [],
    verificationActivity: [],
    timeline: [],
    approvals: [],
    composer: "",
    isLoadingWorkspace: false,
    isLoadingSession: false,
    lastError: null,
    gitStatus: null,
    changes: {
      entries: [],
      added: [],
      modified: [],
      deleted: [],
      additions: 0,
      deletions: 0,
      binaryCount: 0,
    },
    selectedPath: null,
    fileChange: null,
    fileView: null,
    aggregateDiff: null,
    isLoadingChanges: false,
    isChangesTruncated: false,
    isLoadingFile: false,
    checkpoints: [],
    restoringCheckpointId: null,
    lastRestore: null,
    terminal: null,
    isStartingTerminal: false,
    terminalExit: null,
  });
}

describe("terminal sessions", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    resetStore();
    connectRuntime.mockResolvedValue("client-1");
    requestRuntime.mockImplementation(async (_clientId: string, method: string) => {
      if (method === "rpc.initialize") return response({ version: 1 });
      if (method === "workspace.open") {
        return response({
          current_directory: "/repo",
          repository_root: "/repo",
          languages: [],
          manifests: [],
          instructions: [],
          configuration: { package_manager: null, commands: {}, source: null },
        });
      }
      if (method === "session.list") return response([]);
      if (method === "git.status") {
        return response({
          repository_root: "/repo",
          branch: "main",
          head: "abc",
          is_clean: true,
          changed_files: [],
          staged_files: [],
          unstaged_files: [],
          untracked_files: [],
        });
      }
      if (method === "git.diff") return response({ unstaged: "", staged: "" });
      if (method === "checkpoint.list") return response([]);
      if (method === "terminal.open") return response(TERMINAL);
      if (method === "terminal.write") return response({ written: 1 });
      if (method === "terminal.resize") return response({ ...TERMINAL, cols: 120, rows: 40 });
      if (method === "terminal.close") return response({ closed: true });
      return response({});
    });
  });

  it("opens a terminal declaring a human origin", async () => {
    await useDesktopStore.getState().connect("127.0.0.1:4545", "/repo");

    await useDesktopStore.getState().startTerminal(100, 30);

    expect(requestRuntime).toHaveBeenCalledWith("client-1", "terminal.open", {
      origin: "human",
      cols: 100,
      rows: 30,
    });
    expect(useDesktopStore.getState().terminal).toMatchObject({ id: "pty-1", origin: "human" });
    expect(useDesktopStore.getState().isStartingTerminal).toBe(false);
  });

  it("refuses to start a second terminal while one is live", async () => {
    await useDesktopStore.getState().connect("127.0.0.1:4545", "/repo");
    await useDesktopStore.getState().startTerminal();
    requestRuntime.mockClear();

    await useDesktopStore.getState().startTerminal();

    expect(requestRuntime).not.toHaveBeenCalledWith("client-1", "terminal.open", expect.anything());
    expect(useDesktopStore.getState().terminal?.id).toBe("pty-1");
  });

  it("streams keystrokes to the runtime", async () => {
    await useDesktopStore.getState().connect("127.0.0.1:4545", "/repo");
    await useDesktopStore.getState().startTerminal();

    await useDesktopStore.getState().writeTerminal("ls\r");

    expect(requestRuntime).toHaveBeenCalledWith("client-1", "terminal.write", {
      terminal_id: "pty-1",
      data: "ls\r",
    });
  });

  it("reports a resize to the runtime", async () => {
    await useDesktopStore.getState().connect("127.0.0.1:4545", "/repo");
    await useDesktopStore.getState().startTerminal();

    await useDesktopStore.getState().resizeTerminal(120, 40);

    expect(requestRuntime).toHaveBeenCalledWith("client-1", "terminal.resize", {
      terminal_id: "pty-1",
      cols: 120,
      rows: 40,
    });
  });

  it("ignores an invalid resize instead of sending it", async () => {
    await useDesktopStore.getState().connect("127.0.0.1:4545", "/repo");
    await useDesktopStore.getState().startTerminal();
    requestRuntime.mockClear();

    await useDesktopStore.getState().resizeTerminal(0, 0);

    expect(requestRuntime).not.toHaveBeenCalledWith("client-1", "terminal.resize", expect.anything());
  });

  it("streams output to subscribers rather than buffering it in state", async () => {
    await useDesktopStore.getState().connect("127.0.0.1:4545", "/repo");
    await useDesktopStore.getState().startTerminal();
    const received: string[] = [];
    const stop = subscribeTerminalOutput((data) => received.push(data));

    useDesktopStore
      .getState()
      .handleServerMessage(notification("terminal.output", { terminal_id: "pty-1", data: "hello\r\n" }));
    useDesktopStore
      .getState()
      .handleServerMessage(notification("terminal.output", { terminal_id: "pty-1", data: "world" }));
    stop();

    expect(received).toEqual(["hello\r\n", "world"]);
    // Output is a stream, so it must not be retained in store state.
    expect(Object.keys(useDesktopStore.getState())).not.toContain("terminalOutput");
  });

  it("clears the terminal when the runtime reports it exited", async () => {
    await useDesktopStore.getState().connect("127.0.0.1:4545", "/repo");
    await useDesktopStore.getState().startTerminal();

    useDesktopStore.getState().handleServerMessage(
      notification("terminal.exited", { terminal_id: "pty-1", exit_code: 0, reason: "exited" }),
    );

    const state = useDesktopStore.getState();
    expect(state.terminal).toBeNull();
    expect(state.terminalExit).toEqual({ terminalId: "pty-1", exitCode: 0, reason: "exited" });
  });

  it("ignores an exit notice for a different terminal", async () => {
    await useDesktopStore.getState().connect("127.0.0.1:4545", "/repo");
    await useDesktopStore.getState().startTerminal();

    useDesktopStore.getState().handleServerMessage(
      notification("terminal.exited", { terminal_id: "pty-other", exit_code: 1, reason: "exited" }),
    );

    expect(useDesktopStore.getState().terminal?.id).toBe("pty-1");
    expect(useDesktopStore.getState().terminalExit).toBeNull();
  });

  it("notifies exit subscribers", async () => {
    await useDesktopStore.getState().connect("127.0.0.1:4545", "/repo");
    await useDesktopStore.getState().startTerminal();
    const seen: string[] = [];
    const stop = subscribeTerminalExit((exit) => seen.push(exit.reason));

    useDesktopStore.getState().handleServerMessage(
      notification("terminal.exited", { terminal_id: "pty-1", exit_code: null, reason: "closed" }),
    );
    stop();

    expect(seen).toEqual(["closed"]);
  });

  it("closes the terminal through the runtime", async () => {
    await useDesktopStore.getState().connect("127.0.0.1:4545", "/repo");
    await useDesktopStore.getState().startTerminal();

    await useDesktopStore.getState().closeTerminal();

    expect(requestRuntime).toHaveBeenCalledWith("client-1", "terminal.close", {
      terminal_id: "pty-1",
    });
    expect(useDesktopStore.getState().terminal).toBeNull();
  });

  it("surfaces a runtime refusal to open a terminal", async () => {
    requestRuntime.mockImplementation(async (_clientId: string, method: string) => {
      if (method === "terminal.open") {
        return {
          version: 1,
          id: "request-1",
          ok: false,
          error: { code: "runtime_error", message: "invalid_origin: terminal origin must be 'human'" },
        } as RpcResponse<never>;
      }
      if (method === "rpc.initialize") return response({ version: 1 });
      if (method === "workspace.open") {
        return response({
          current_directory: "/repo",
          repository_root: "/repo",
          languages: [],
          manifests: [],
          instructions: [],
          configuration: { package_manager: null, commands: {}, source: null },
        });
      }
      if (method === "session.list") return response([]);
      if (method === "git.status") {
        return response({
          repository_root: "/repo",
          branch: "main",
          head: "abc",
          is_clean: true,
          changed_files: [],
          staged_files: [],
          unstaged_files: [],
          untracked_files: [],
        });
      }
      if (method === "git.diff") return response({ unstaged: "", staged: "" });
      if (method === "checkpoint.list") return response([]);
      return response({});
    });

    await useDesktopStore.getState().connect("127.0.0.1:4545", "/repo");
    await useDesktopStore.getState().startTerminal();

    expect(useDesktopStore.getState().terminal).toBeNull();
    expect(useDesktopStore.getState().lastError).toContain("invalid_origin");
  });

  it("rejects a malformed terminal session", async () => {
    requestRuntime.mockImplementation(async (_clientId: string, method: string) => {
      if (method === "terminal.open") return response({ id: 42 });
      if (method === "rpc.initialize") return response({ version: 1 });
      if (method === "workspace.open") {
        return response({
          current_directory: "/repo",
          repository_root: "/repo",
          languages: [],
          manifests: [],
          instructions: [],
          configuration: { package_manager: null, commands: {}, source: null },
        });
      }
      if (method === "session.list") return response([]);
      if (method === "git.status") {
        return response({
          repository_root: "/repo",
          branch: "main",
          head: "abc",
          is_clean: true,
          changed_files: [],
          staged_files: [],
          unstaged_files: [],
          untracked_files: [],
        });
      }
      if (method === "git.diff") return response({ unstaged: "", staged: "" });
      if (method === "checkpoint.list") return response([]);
      return response({});
    });

    await useDesktopStore.getState().connect("127.0.0.1:4545", "/repo");
    await useDesktopStore.getState().startTerminal();

    expect(useDesktopStore.getState().terminal).toBeNull();
    expect(useDesktopStore.getState().lastError).toContain("malformed");
  });
});

describe("terminal helpers", () => {
  it("validates a terminal session shape", () => {
    expect(isPtyInfo(TERMINAL)).toBe(true);
    expect(isPtyInfo({ id: "pty-1" })).toBe(false);
    expect(isPtyInfo(null)).toBe(false);
  });

  it("explains why a terminal ended", () => {
    expect(describeExit({ terminalId: "a", exitCode: 0, reason: "exited" })).toBe(
      "Terminal exited with code 0.",
    );
    expect(describeExit({ terminalId: "a", exitCode: null, reason: "exited" })).toBe("Terminal ended.");
    expect(describeExit({ terminalId: "a", exitCode: null, reason: "closed" })).toBe("Terminal closed.");
  });
});
