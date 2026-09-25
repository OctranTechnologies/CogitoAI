import { beforeEach, describe, expect, it, vi } from "vitest";
import { useDesktopStore } from "./store";
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
});
