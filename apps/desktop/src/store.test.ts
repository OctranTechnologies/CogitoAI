import { beforeEach, describe, expect, it, vi } from "vitest";
import { useDesktopStore } from "./store";
import { summarizeChanges } from "./lib/changes";
import type { HarnessEvent, RpcResponse, ServerMessage } from "./lib/rpc";

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

function event(id: string, type: string, data: Record<string, unknown>, timestamp = 1): HarnessEvent {
  return {
    schema_version: 1,
    event_id: id,
    session_id: "session-1",
    timestamp,
    event_type: type,
    parent_id: null,
    correlation_id: null,
    payload: { type, data },
  };
}

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
    changes: summarizeChanges([]),
    selectedPath: null,
    fileChange: null,
    fileView: null,
    aggregateDiff: null,
    isLoadingChanges: false,
    isLoadingFile: false,
    checkpoints: [],
    restoringCheckpointId: null,
    lastRestore: null,
  });
}

describe("desktop runtime store", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    resetStore();
    connectRuntime.mockResolvedValue("client-1");
    requestRuntime.mockImplementation(async (_clientId: string, method: string) => {
      if (method === "rpc.initialize") return response({ version: 1 });
      if (method === "workspace.open") return response({ current_directory: "/repo", repository_root: "/repo", languages: ["Rust"], manifests: [], instructions: [], configuration: { package_manager: "cargo", commands: {}, source: null } });
      if (method === "session.list") return response([]);
      if (method === "session.create") return response({ session: { id: "session-1" } });
      if (method === "agent.send") return response({ run_id: "run-1" });
      if (method === "session.resume") return response({});
      if (method === "session.inspect") return response({ session: { id: "session-1", workspace_root: "/repo", events: [] }, warnings: [] });
      return response({});
    });
  });

  it("drives a complete mock task from the desktop store", async () => {
    await useDesktopStore.getState().connect("127.0.0.1:4545", "/repo");
    await useDesktopStore.getState().sendMessage("inspect the repository");
    const state = useDesktopStore.getState();
    expect(state.activeRunId).toBe("run-1");
    expect(state.runPhase).toBe("pending");

    state.handleServerMessage(notification("agent.event", { event: event("a", "tool.requested", { tool: "list_directory", arguments: { path: "." } }, 1) }));
    state.handleServerMessage(notification("agent.event", { event: event("b", "tool.started", { tool: "list_directory" }, 2) }));
    state.handleServerMessage(notification("agent.event", { event: event("c", "tool.output", { tool: "list_directory", output: "README.md" }, 3) }));
    state.handleServerMessage(notification("agent.event", { event: event("d", "tool.completed", { tool: "list_directory" }, 4) }));
    state.handleServerMessage(notification("agent.event", { event: event("e", "assistant.delta", { text: "Complete" }, 5) }));
    state.handleServerMessage(notification("agent.event", { event: event("f", "assistant.message", { text: "Complete" }, 6) }));
    state.handleServerMessage(notification("agent.completed", { run_id: "run-1" }));

    const completed = useDesktopStore.getState();
    expect(completed.runPhase).toBe("completed");
    expect(completed.messages.map((message) => message.text)).toEqual(["inspect the repository", "Complete"]);
    expect(completed.toolActivity[0]).toMatchObject({ name: "list_directory", state: "succeeded", output: "README.md" });
    expect(completed.timeline.map((entry) => entry.eventType)).toContain("tool.completed");
  });

  it("resolves allow-once and deny-once approval requests", async () => {
    await useDesktopStore.getState().connect("127.0.0.1:4545", "/repo");
    const approval = { approval_id: "approval-1", tool: { name: "write_file", arguments: { path: "blocked.txt" } } };
    useDesktopStore.getState().handleServerMessage(notification("approval.request", approval));

    expect(useDesktopStore.getState().approvals).toHaveLength(1);
    await useDesktopStore.getState().approve("approval-1");
    expect(useDesktopStore.getState().approvals).toHaveLength(0);
    expect(requestRuntime).toHaveBeenCalledWith("client-1", "agent.approve", { approval_id: "approval-1" });

    useDesktopStore.getState().handleServerMessage(notification("approval.request", { ...approval, approval_id: "approval-2" }));
    await useDesktopStore.getState().deny("approval-2");
    expect(useDesktopStore.getState().approvals).toHaveLength(0);
    expect(requestRuntime).toHaveBeenCalledWith("client-1", "agent.deny", { approval_id: "approval-2" });
  });

  it("surfaces cancellation and runtime errors", async () => {
    await useDesktopStore.getState().connect("127.0.0.1:4545", "/repo");
    await useDesktopStore.getState().sendMessage("cancel me");
    await useDesktopStore.getState().cancel();
    expect(useDesktopStore.getState().runPhase).toBe("cancelling");
    useDesktopStore.getState().handleServerMessage(notification("agent.failed", { error: { code: "cancelled", message: "agent cancelled" } }));
    expect(useDesktopStore.getState().runPhase).toBe("cancelled");
    expect(useDesktopStore.getState().lastError).toBe("agent cancelled");
  });

  it("rehydrates a persisted session on resume", async () => {
    useDesktopStore.setState({ activeSessionId: "session-1", clientId: "client-1" });
    requestRuntime.mockImplementation(async (_clientId: string, method: string) => {
      if (method === "session.resume") return response({});
      if (method === "session.inspect") return response({ session: { id: "session-1", workspace_root: "/repo", events: [event("a", "user.message", { text: "persisted task" }), event("b", "assistant.message", { text: "persisted answer" })] }, warnings: [] });
      return response({});
    });

    await useDesktopStore.getState().resumeSession("session-1");

    const resumed = useDesktopStore.getState();
    expect(resumed.isLoadingSession).toBe(false);
    expect(resumed.messages.map((message) => message.text)).toEqual(["persisted task", "persisted answer"]);
    expect(resumed.events).toHaveLength(2);
  });

  describe("code changes and checkpoints", () => {
    function mockWorkspace(method: string, params: Record<string, unknown>) {
      if (method === "git.status") {
        return response({
          repository_root: "/repo",
          branch: "main",
          head: "abc123",
          is_clean: false,
          changed_files: ["src/edited.rs", "src/added.rs"],
          staged_files: [],
          unstaged_files: ["src/edited.rs"],
          untracked_files: ["src/added.rs"],
        });
      }
      if (method === "git.diff") return response({ unstaged: "diff --git a/src/edited.rs", staged: "" });
      if (method === "checkpoint.list") {
        return response([
          {
            id: "checkpoint-1",
            session_id: "session-1",
            working_directory: "/repo",
            created_at: "1700000000000",
            reference: "abc123",
            baseline_file_count: 4,
            recorded_changes: ["src/edited.rs"],
          },
        ]);
      }
      if (method === "git.file_diff" && params.path === "src/edited.rs") {
        return response({
          path: "src/edited.rs",
          language: "rust",
          kind: "modified",
          original: "fn main() {}\n",
          modified: "fn main() { run(); }\n",
          patch: "diff",
          additions: 1,
          deletions: 1,
          is_binary: false,
          truncated: false,
        });
      }
      if (method === "git.file_diff" && params.path === "src/added.rs") {
        return response({
          path: "src/added.rs",
          language: "rust",
          kind: "added",
          original: "",
          modified: "pub fn run() {}\n",
          patch: "diff",
          additions: 1,
          deletions: 0,
          is_binary: false,
          truncated: false,
        });
      }
      if (method === "checkpoint.undo") {
        return response({ checkpoint_id: "checkpoint-1", restored_files: ["/repo/src/edited.rs"], conflicts: [] });
      }
      if (method === "git.status" && useDesktopStore.getState().lastRestore !== null) {
        return response({
          repository_root: "/repo",
          branch: "main",
          head: "abc123",
          is_clean: true,
          changed_files: [],
          staged_files: [],
          unstaged_files: [],
          untracked_files: [],
        });
      }
      return response({});
    }

    beforeEach(() => {
      requestRuntime.mockImplementation(async (_clientId: string, method: string, params = {}) =>
        mockWorkspace(method, params),
      );
    });

    it("loads git changes, per-file diffs, and checkpoints from the runtime", async () => {
      await useDesktopStore.getState().connect("127.0.0.1:4545", "/repo");

      const state = useDesktopStore.getState();
      expect(state.gitStatus?.branch).toBe("main");
      expect(state.changes.entries.map((entry) => entry.path)).toEqual(["src/edited.rs", "src/added.rs"]);
      expect(state.changes.added.map((entry) => entry.path)).toEqual(["src/added.rs"]);
      expect(state.changes.modified.map((entry) => entry.path)).toEqual(["src/edited.rs"]);
      expect(state.changes.additions).toBe(2);
      expect(state.checkpoints).toHaveLength(1);
      expect(state.checkpoints[0].id).toBe("checkpoint-1");
      expect(state.checkpoints[0].affectedFiles).toEqual(["src/edited.rs"]);
      expect(state.aggregateDiff?.unstaged).toContain("src/edited.rs");
    });

    it("loads a per-file diff when a changed file is selected", async () => {
      await useDesktopStore.getState().connect("127.0.0.1:4545", "/repo");

      await useDesktopStore.getState().selectFile("src/edited.rs");

      const state = useDesktopStore.getState();
      expect(state.selectedPath).toBe("src/edited.rs");
      expect(state.fileChange).toMatchObject({
        kind: "modified",
        original: "fn main() {}\n",
        modified: "fn main() { run(); }\n",
        additions: 1,
        deletions: 1,
      });
      expect(state.isLoadingFile).toBe(false);
    });

    it("restores a checkpoint through the runtime and refreshes changes", async () => {
      await useDesktopStore.getState().connect("127.0.0.1:4545", "/repo");

      await useDesktopStore.getState().restoreCheckpoint("checkpoint-1");

      expect(requestRuntime).toHaveBeenCalledWith("client-1", "checkpoint.undo", {
        checkpoint_id: "checkpoint-1",
      });
      const state = useDesktopStore.getState();
      expect(state.restoringCheckpointId).toBeNull();
      expect(state.lastRestore).toEqual({
        checkpoint_id: "checkpoint-1",
        restored_files: ["/repo/src/edited.rs"],
        conflicts: [],
      });
    });

    it("surfaces a runtime restore conflict without touching the frontend", async () => {
      requestRuntime.mockImplementation(async (clientId: string, method: string) => {
        if (method === "git.status" || method === "git.diff" || method === "checkpoint.list" || method === "git.file_diff") {
          return mockWorkspace(method, {});
        }
        if (method === "checkpoint.undo") {
          return {
            version: 1,
            id: "request-1",
            ok: false,
            error: { code: "runtime_error", message: "restore conflict; no files were changed" },
          } as RpcResponse<never>;
        }
        if (method === "rpc.initialize") return response({ version: 1 });
        if (method === "workspace.open") {
          return response({ current_directory: "/repo", repository_root: "/repo", languages: [], manifests: [], instructions: [], configuration: { package_manager: null, commands: {}, source: null } });
        }
        if (method === "session.list") return response([]);
        return response({});
      });

      await useDesktopStore.getState().connect("127.0.0.1:4545", "/repo");
      await useDesktopStore.getState().restoreCheckpoint("checkpoint-1");

      const state = useDesktopStore.getState();
      expect(state.lastError).toContain("restore conflict");
      expect(state.restoringCheckpointId).toBeNull();
      expect(state.lastRestore).toBeNull();
    });

    it("refreshes the changes panel when a runtime event reports a file change", async () => {
      await useDesktopStore.getState().connect("127.0.0.1:4545", "/repo");
      requestRuntime.mockClear();

      useDesktopStore
        .getState()
        .handleServerMessage(notification("agent.event", { event: event("z", "file.changed", { path: "src/edited.rs" }) }));
      await vi.waitFor(() =>
        expect(requestRuntime).toHaveBeenCalledWith("client-1", "git.file_diff", { path: "src/edited.rs" }),
      );
      expect(requestRuntime).toHaveBeenCalledWith("client-1", "git.status", {});
    });
  });
});
