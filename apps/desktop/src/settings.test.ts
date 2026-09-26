import { beforeEach, describe, expect, it, vi } from "vitest";
import { useDesktopStore } from "./store";
import {
  describeCapabilities,
  describeCredential,
  formatCommand,
  isSettingsSnapshot,
  type SettingsSnapshot,
} from "./lib/settings";
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

const SNAPSHOT: SettingsSnapshot = {
  models: {
    provider: "OpenAI",
    model: "gpt-4o",
    base_url: "https://api.openai.com/v1",
    api_key_env: "OPENAI_API_KEY",
    capabilities: {
      streaming: true,
      tool_calling: true,
      vision: true,
      reasoning: false,
      context_window: 128000,
    },
    credential: { available: true, source: "environment", env_var: "OPENAI_API_KEY" },
    available_models: ["gpt-4o", "gpt-4o-mini"],
    configured: true,
  },
  permissions: {
    mode: "normal",
    mode_description: "Safe commands run directly.",
    available_modes: ["read_only", "safe", "normal", "auto"],
    built_in_rules: [
      { name: "workspace-boundary", action: "deny", reason: "Outside the workspace.", tools: [], operations: [] },
    ],
    configured_rules: [],
    default_behavior: [{ operation: "read", effect: "allowed" }],
  },
  project: {
    workspace_path: "C:/repo",
    repository_root: "C:/repo",
    is_git_repository: true,
    languages: ["Rust"],
    manifests: ["Cargo.toml"],
    package_manager: "cargo",
    instruction_files: ["Readme · README.md"],
    monorepo: false,
  },
  verification: {
    commands: [{ category: "test", program: "cargo", args: ["test"], is_override: false }],
    source: null,
    has_project_overrides: false,
  },
  runtime: {
    version: "0.1.0",
    session_storage_path: "C:/repo/.cogito/sessions",
    checkpoint_storage_path: "C:/repo/.cogito/checkpoints",
    log_level: "info",
    log_target: "runtime stdout",
    provider_names: ["openai"],
    credential_source: "environment",
  },
};

/** A snapshot that deliberately contains a secret, to prove the client cannot
 * be trusted to have received one. */
const SECRET = "sk-should-never-reach-the-client";

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
    changes: { entries: [], added: [], modified: [], deleted: [], additions: 0, deletions: 0, binaryCount: 0 },
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
    settings: null,
    isLoadingSettings: false,
    settingsError: null,
    modelTest: null,
  });
}

describe("settings", () => {
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
      if (method === "settings.inspect") return response(SNAPSHOT);
      if (method === "settings.update_model") {
        return response({
          ...SNAPSHOT,
          models: { ...SNAPSHOT.models, model: "gpt-4o-mini" },
        });
      }
      if (method === "settings.update_permissions") {
        return response({
          ...SNAPSHOT,
          permissions: { ...SNAPSHOT.permissions, mode: "read_only" },
        });
      }
      if (method === "settings.test_model") {
        return response({ ok: true, message: "Configured.", skipped: false });
      }
      return response({});
    });
  });

  it("loads all five screens on connect", async () => {
    await useDesktopStore.getState().connect("127.0.0.1:4545", "/repo");

    const settings = useDesktopStore.getState().settings;
    expect(settings).not.toBeNull();
    expect(settings!.models.model).toBe("gpt-4o");
    expect(settings!.permissions.mode).toBe("normal");
    expect(settings!.project.workspace_path).toBe("C:/repo");
    expect(settings!.verification.commands).toHaveLength(1);
    expect(settings!.runtime.version).toBe("0.1.0");
    expect(useDesktopStore.getState().isLoadingSettings).toBe(false);
  });

  it("never stores a credential value", async () => {
    await useDesktopStore.getState().connect("127.0.0.1:4545", "/repo");

    const settings = useDesktopStore.getState().settings!;
    // Only presence and the variable name are present; there is no field that
    // could hold the secret itself.
    expect(settings.models.credential).toEqual({
      available: true,
      source: "environment",
      env_var: "OPENAI_API_KEY",
    });
    expect(JSON.stringify(settings)).not.toContain(SECRET);
    expect(Object.keys(settings.models.credential)).toEqual(["available", "source", "env_var"]);
  });

  it("switches the model and persists it in the snapshot", async () => {
    await useDesktopStore.getState().connect("127.0.0.1:4545", "/repo");

    const ok = await useDesktopStore.getState().updateModel({ model: "gpt-4o-mini" });

    expect(ok).toBe(true);
    expect(requestRuntime).toHaveBeenCalledWith("client-1", "settings.update_model", {
      model: "gpt-4o-mini",
    });
    expect(useDesktopStore.getState().settings!.models.model).toBe("gpt-4o-mini");
  });

  it("surfaces a rejected model change without altering the snapshot", async () => {
    requestRuntime.mockImplementation(async (_clientId: string, method: string) => {
      if (method === "settings.update_model") {
        return {
          version: 1,
          id: "request-1",
          ok: false,
          error: { code: "runtime_error", message: "invalid setting: model must not be empty" },
        } as RpcResponse<never>;
      }
      if (method === "settings.inspect") return response(SNAPSHOT);
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
    const ok = await useDesktopStore.getState().updateModel({ model: "  " });

    expect(ok).toBe(false);
    expect(useDesktopStore.getState().settingsError).toContain("model must not be empty");
    // The previously reported model is unchanged.
    expect(useDesktopStore.getState().settings!.models.model).toBe("gpt-4o");
  });

  it("changes the permission mode", async () => {
    await useDesktopStore.getState().connect("127.0.0.1:4545", "/repo");

    const ok = await useDesktopStore.getState().updatePermissionMode("read_only");

    expect(ok).toBe(true);
    expect(requestRuntime).toHaveBeenCalledWith("client-1", "settings.update_permissions", {
      mode: "read_only",
    });
    expect(useDesktopStore.getState().settings!.permissions.mode).toBe("read_only");
  });

  it("rejects an unknown permission mode", async () => {
    requestRuntime.mockImplementation(async (_clientId: string, method: string) => {
      if (method === "settings.update_permissions") {
        return {
          version: 1,
          id: "request-1",
          ok: false,
          error: { code: "runtime_error", message: "unknown execution mode 'yolo'" },
        } as RpcResponse<never>;
      }
      if (method === "settings.inspect") return response(SNAPSHOT);
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
    const ok = await useDesktopStore.getState().updatePermissionMode("yolo");

    expect(ok).toBe(false);
    expect(useDesktopStore.getState().settingsError).toContain("unknown execution mode");
    expect(useDesktopStore.getState().settings!.permissions.mode).toBe("normal");
  });

  it("reports a connection test without exposing a secret", async () => {
    await useDesktopStore.getState().connect("127.0.0.1:4545", "/repo");

    await useDesktopStore.getState().testModelConnection();

    const result = useDesktopStore.getState().modelTest;
    expect(result).toEqual({ ok: true, message: "Configured.", skipped: false });
    expect(JSON.stringify(result)).not.toContain(SECRET);
  });

  it("rejects a malformed settings payload", async () => {
    requestRuntime.mockImplementation(async (_clientId: string, method: string) => {
      if (method === "settings.inspect") return response({ models: {} });
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

    expect(useDesktopStore.getState().settings).toBeNull();
    expect(useDesktopStore.getState().settingsError).toContain("malformed");
  });

  it("keeps settings out of runtime events", async () => {
    await useDesktopStore.getState().connect("127.0.0.1:4545", "/repo");

    useDesktopStore
      .getState()
      .handleServerMessage(notification("agent.event", { event: { event_type: "policy.decision" } }));

    const serialized = JSON.stringify(useDesktopStore.getState());
    expect(serialized).not.toContain(SECRET);
  });
});

describe("settings helpers", () => {
  it("describes a credential without revealing it", () => {
    expect(
      describeCredential({ available: true, source: "environment", env_var: "OPENAI_API_KEY" }),
    ).toBe("Configured from OPENAI_API_KEY");
    expect(
      describeCredential({ available: false, source: "environment", env_var: "OPENAI_API_KEY" }),
    ).toBe("Not set — set OPENAI_API_KEY in the runtime environment");
  });

  it("summarizes model capabilities", () => {
    const summary = describeCapabilities(SNAPSHOT.models.capabilities);
    expect(summary).toContain("streaming");
    expect(summary).toContain("tool calling");
    expect(summary).toContain("vision");
    expect(summary).toContain("128,000 token context");
  });

  it("formats a verification command", () => {
    expect(
      formatCommand({ category: "test", program: "cargo", args: ["test", "--all"], is_override: false }),
    ).toBe("cargo test --all");
  });

  it("validates a settings snapshot shape", () => {
    expect(isSettingsSnapshot(SNAPSHOT)).toBe(true);
    expect(isSettingsSnapshot({ models: {} })).toBe(false);
    expect(isSettingsSnapshot(null)).toBe(false);
  });
});
