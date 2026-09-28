import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { createElement } from "react";
import { SessionWorkspace } from "./session-workspace";
import { buildActivityStream, type ActivityBlock } from "../lib/activity";
import type { HarnessEvent } from "../lib/rpc";
import type { SessionWorkspaceProps } from "./session-workspace";

afterEach(cleanup);

let counter = 0;
function event(event_type: string, data: Record<string, unknown>, timestamp?: number): HarnessEvent {
  counter += 1;
  return {
    schema_version: 1,
    event_id: `e${counter}`,
    session_id: "s1",
    timestamp: timestamp ?? 1_700_000_000_000 + counter,
    event_type,
    parent_id: null,
    correlation_id: null,
    payload: { type: event_type, data },
  };
}

function toolCall(tool: string, args: Record<string, unknown>) {
  return [event("tool.requested", { tool, arguments: args }), event("tool.started", { tool })];
}

const MODELS = {
  provider: "openai",
  provider_id: "openai",
  model: "gpt-4o",
  base_url: "https://api.openai.com/v1",
  api_key_env: "OPENAI_API_KEY",
  capabilities: {
    text_input: true,
    image_input: false,
    streaming: true,
    tool_calling: true,
    parallel_tool_calls: false,
    vision: false,
    reasoning: true,
    configurable_reasoning_effort: false,
    context_window: 128000,
    max_output_tokens: null,
    structured_output: false,
  },
  credential: { available: true, source: "environment" as const, env_var: "OPENAI_API_KEY" },
  available_models: ["gpt-4o", "gpt-4o-mini"],
  reasoning_levels: [],
  reasoning_effort: null,
  configured: true,
};

const PERMISSIONS = {
  mode: "normal",
  mode_description: "Allows project edits and known-safe commands.",
  available_modes: ["read_only", "safe", "normal", "auto"],
  built_in_rules: [],
  configured_rules: [],
  default_behavior: [],
};

function setup(overrides: Partial<SessionWorkspaceProps> = {}) {
  const props: SessionWorkspaceProps = {
    events: [],
    hasConversation: true,
    approvals: [],
    connected: true,
    runPhase: "idle",
    running: false,
    projectName: "C:/repo",
    branch: "main",
    models: MODELS,
    modelCatalog: [],
    providerCredentials: [],
    isLoadingModelCatalog: false,
    permissions: PERMISSIONS,
    composer: {
      value: "",
      onChange: vi.fn(),
      onSubmit: vi.fn(),
      onCancel: vi.fn(),
      workspacePath: "C:/repo",
      onChooseWorkspace: vi.fn(),
      pendingMode: null,
      onSelectModel: vi.fn(),
      onRefreshModelCatalog: vi.fn(),
      onConnectProvider: vi.fn(),
      onSelectReasoning: vi.fn(),
      onSelectMode: vi.fn(),
    },
    onApprove: vi.fn(),
    onDeny: vi.fn(),
    onSelectFile: vi.fn(),
    inspector: {
      open: false,
      tab: "changes",
      onTabChange: vi.fn(),
      onToggle: vi.fn(),
      onClose: vi.fn(),
    },
    changes: {
      entries: [],
      selectedPath: null,
      fileChange: null,
      fileView: null,
      isLoading: false,
      isTruncated: false,
      totalChanged: 0,
      isGitWorkspace: true,
    },
    onClearFile: vi.fn(),
    checkpoints: [],
    restoringId: null,
    lastRestore: null,
    restoreDisabled: false,
    onRestore: vi.fn(),
    ...overrides,
  };
  render(createElement(SessionWorkspace, props));
  return props;
}

beforeEach(() => {
  counter = 0;
  vi.clearAllMocks();
});

describe("session header", () => {
  it("shows only what the runtime reported", () => {
    setup();
    expect(screen.getByTestId("header-project").textContent).toBe("C:/repo");
    expect(screen.getByTestId("header-branch").textContent).toBe("main");
    expect(screen.getByTestId("header-model").textContent).toContain("openai/gpt-4o");
    expect(screen.getByTestId("header-mode").textContent).toBe("Normal");
    expect(screen.getByTestId("header-runtime").textContent).toBe("connected");
  });

  it("omits a branch the runtime did not report rather than inventing one", () => {
    setup({ branch: null });
    expect(screen.queryByTestId("header-branch")).toBeNull();
  });

  it("shows disconnected rather than pretending a runtime is present", () => {
    setup({ connected: false });
    expect(screen.getByTestId("header-runtime").textContent).toBe("disconnected");
  });

  it("reports in-flight work and failures without dominating the line", () => {
    setup({
      events: [
        event("tool.requested", { tool: "read_file", arguments: { path: "a.ts" } }),
        event("tool.requested", { tool: "grep", arguments: { pattern: "x" } }),
        event("tool.started", { tool: "grep" }),
        event("tool.started", { tool: "read_file" }),
        event("tool.completed", { tool: "read_file" }),
        event("tool.requested", { tool: "shell", arguments: { command: "pnpm test" } }),
        event("tool.started", { tool: "shell" }),
        event("tool.failed", { tool: "shell", error: "boom" }),
      ],
    });
    expect(screen.getByTestId("header-active-tools").textContent).toBe("1 in flight");
    expect(screen.getByTestId("header-failures").textContent).toBe("1 failed");
  });
});

describe("conversation stream", () => {
  it("renders each kind of block from the runtime event log", () => {
    setup({
      events: [
        event("user.message", { text: "fix the parser" }),
        ...toolCall("read_file", { path: "src/auth.ts" }),
        event("tool.completed", { tool: "read_file" }),
        event("assistant.message", { text: "Fixed it." }),
        event("verification.result", {
          command: "pnpm test",
          category: "test",
          passed: true,
          duration_ms: 1200,
          exit_code: 0,
          output: "",
          diagnostics: ["a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m", "n"],
        }),
      ],
    });

    const stream = screen.getByTestId("activity-stream");
    expect(within(stream).getByText("You")).toBeTruthy();
    expect(within(stream).getByText("fix the parser")).toBeTruthy();
    expect(within(stream).getByText("Reading src/auth.ts")).toBeTruthy();
    expect(within(stream).getByText("Fixed it.")).toBeTruthy();
    expect(within(stream).getByText(/14 checks passed · pnpm test/)).toBeTruthy();
  });

  it("renders a failure as a failure", () => {
    setup({
      events: [...toolCall("shell", { command: "pnpm test" }), event("tool.failed", { tool: "shell", error: "exit 1" })],
    });
    const row = screen.getByText("Running pnpm test").closest("[data-testid^='tool-']")!;
    expect(row.getAttribute("data-phase")).toBe("failed");
  });

  it("renders a run failure as an error block", () => {
    setup({ events: [event("session.failed", { error: "model unavailable" })] });
    expect(screen.getByText("Run failed")).toBeTruthy();
    expect(screen.getByText("model unavailable")).toBeTruthy();
  });

  it("keeps prose unwrapped in bubbles so a long brief keeps its width", () => {
    setup({ events: [event("user.message", { text: "a".repeat(400) })] });
    const user = screen.getByText("a".repeat(400));
    expect(user.className).toContain("whitespace-pre-wrap");
    expect(user.className).not.toContain("rounded");
  });
});

describe("tool rows", () => {
  it("expands and collapses to reveal arguments and output", () => {
    setup({
      events: [
        event("tool.requested", { tool: "read_file", arguments: { path: "src/auth.ts" } }),
        event("tool.started", { tool: "read_file" }),
        event("tool.output", { tool: "read_file", output: "file contents" }),
        event("tool.completed", { tool: "read_file" }),
      ],
    });

    const toggle = screen.getByRole("button", { name: /Reading src\/auth\.ts/ });
    expect(toggle.getAttribute("aria-expanded")).toBe("false");
    expect(screen.queryByText("file contents")).toBeNull();

    fireEvent.click(toggle);
    expect(toggle.getAttribute("aria-expanded")).toBe("true");
    expect(screen.getByText("file contents")).toBeTruthy();
    expect(screen.getByText("path")).toBeTruthy();

    fireEvent.click(toggle);
    expect(toggle.getAttribute("aria-expanded")).toBe("false");
    expect(screen.queryByText("file contents")).toBeNull();
  });

  it("hides bulky file bodies from the expanded row", () => {
    setup({
      events: [
        event("tool.requested", { tool: "apply_patch", arguments: { path: "a.ts", old_text: "SECRET_OLD", new_text: "SECRET_NEW" } }),
        event("tool.completed", { tool: "apply_patch" }),
      ],
    });
    fireEvent.click(screen.getByRole("button", { name: /Editing a\.ts/ }));
    expect(screen.queryByText(/SECRET_OLD|SECRET_NEW/)).toBeNull();
  });

  it("offers no toggle for a call with nothing to reveal", () => {
    // No arguments and no output, so an expander would open onto nothing.
    setup({ events: toolCall("list_directory", {}) });
    const toggle = screen.getByRole("button", { name: /Listing the workspace/ }) as HTMLButtonElement;
    expect(toggle.disabled).toBe(true);
    expect(toggle.getAttribute("aria-expanded")).toBe("false");
  });

  it("hands a file to the inspector on request", () => {
    const onSelectFile = vi.fn();
    setup({
      onSelectFile,
      events: [
        event("tool.requested", { tool: "read_file", arguments: { path: "src/auth.ts" } }),
        event("tool.completed", { tool: "read_file" }),
      ],
    });
    fireEvent.click(screen.getByRole("button", { name: /Reading src\/auth\.ts/ }));
    fireEvent.click(screen.getByRole("button", { name: /Open in inspector/ }));
    expect(onSelectFile).toHaveBeenCalledWith("src/auth.ts");
  });

  it("distinguishes the four states with a distinct indicator and a readable status", () => {
    setup({
      events: [
        event("tool.requested", { tool: "read_file", arguments: { path: "a.ts" } }),
        event("tool.started", { tool: "read_file" }),
      ],
      approvals: [{ approval_id: "ap1", tool: { name: "read_file", arguments: { path: "a.ts" } } }],
    });
    const row = screen.getByText("Reading a.ts").closest("[data-testid^='tool-']")!;
    expect(row.getAttribute("data-phase")).toBe("awaiting_approval");
    expect(within(row as HTMLElement).getByText("waiting for approval")).toBeTruthy();
  });
});

describe("approval flow", () => {
  it("shows approve and deny on the tool row that is blocked", () => {
    const onApprove = vi.fn();
    const onDeny = vi.fn();
    setup({
      onApprove,
      onDeny,
      events: [event("tool.requested", { tool: "shell", arguments: { command: "rm -rf build" } })],
      approvals: [{ approval_id: "ap1", tool: { name: "shell", arguments: { command: "rm -rf build" } } }],
    });

    fireEvent.click(screen.getByRole("button", { name: "Approve" }));
    expect(onApprove).toHaveBeenCalledWith("ap1");
    fireEvent.click(screen.getByRole("button", { name: "Deny" }));
    expect(onDeny).toHaveBeenCalledWith("ap1");
  });

  it("keeps an orphaned approval answerable", () => {
    const onApprove = vi.fn();
    setup({ onApprove, events: [], approvals: [{ approval_id: "ap9", tool: { name: "shell", arguments: { command: "ls" } } }] });
    expect(screen.getByTestId("approval-ap9")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Approve" }));
    expect(onApprove).toHaveBeenCalledWith("ap9");
  });
});

describe("inspector", () => {
  it("is hidden by default so the conversation keeps the full width", () => {
    setup({ events: [event("user.message", { text: "hi" })] });
    expect(screen.queryByTestId("inspector")).toBeNull();
  });

  it("opens with the section that was asked for", () => {
    setup({ inspector: { open: true, tab: "events", onTabChange: vi.fn(), onToggle: vi.fn(), onClose: vi.fn() } });
    expect(screen.getByTestId("inspector")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Events" }).getAttribute("aria-current")).toBe("true");
  });

  it("shows the raw event payloads so behaviour can be traced", () => {
    setup({
      inspector: { open: true, tab: "events", onTabChange: vi.fn(), onToggle: vi.fn(), onClose: vi.fn() },
      events: [event("tool.requested", { tool: "read_file", arguments: { path: "a.ts" } })],
    });
    fireEvent.click(screen.getByRole("button", { name: /tool\.requested/ }));
    expect(screen.getByText(/"read_file"/)).toBeTruthy();
  });

  it("toggles from the header", () => {
    const onToggle = vi.fn();
    setup({ inspector: { open: false, tab: "changes", onTabChange: vi.fn(), onToggle, onClose: vi.fn() } });
    fireEvent.click(screen.getByRole("button", { name: "Show inspector" }));
    expect(onToggle).toHaveBeenCalled();
  });
});

describe("composer placement", () => {
  it("keeps the composer available while the conversation is active", () => {
    setup({ events: [event("user.message", { text: "hi" })] });
    expect(screen.getByLabelText("Message the agent")).toBeTruthy();
  });
});

describe("responsive inspector layout", () => {
  it("overlays the inspector below the wide-window breakpoint without shrinking the activity column", () => {
    const props = baseProps();
    props.inspector.open = true;
    setup(props);

    const inspector = screen.getByLabelText("Inspector");
    for (const className of ["absolute", "inset-y-0", "right-0", "xl:static"]) {
      expect(inspector.classList.contains(className)).toBe(true);
    }
    expect(screen.getByTestId("activity-scroll")).toBeTruthy();
    expect(screen.getByLabelText("Message the agent")).toBeTruthy();
  });
});

describe("auto-scroll", () => {
  it("follows new activity while the reader is at the bottom", async () => {
    const { rerender } = render(
      createElement(SessionWorkspace, {
        ...baseProps(),
        events: [event("user.message", { text: "one" })],
      }),
    );
    const scroller = screen.getByTestId("activity-scroll");
    Object.defineProperty(scroller, "scrollHeight", { value: 2000, configurable: true });
    Object.defineProperty(scroller, "clientHeight", { value: 500, configurable: true });
    scroller.scrollTop = 1500;
    fireEvent.scroll(scroller);

    await act(async () => {
      rerender(
        createElement(SessionWorkspace, {
          ...baseProps(),
          events: [event("user.message", { text: "one" }), event("assistant.message", { text: "two" })],
        }),
      );
    });

    expect(scroller.scrollTop).toBe(2000);
    expect(screen.queryByRole("button", { name: /Jump to latest/ })).toBeNull();
  });

  it("does not fight the reader who has scrolled up", async () => {
    const first = [event("user.message", { text: "one" })];
    const { rerender } = render(createElement(SessionWorkspace, { ...baseProps(), events: first }));
    const scroller = screen.getByTestId("activity-scroll");
    Object.defineProperty(scroller, "scrollHeight", { value: 2000, configurable: true });
    Object.defineProperty(scroller, "clientHeight", { value: 500, configurable: true });

    // The reader scrolls away from the tail.
    scroller.scrollTop = 200;
    fireEvent.scroll(scroller);

    await act(async () => {
      rerender(
        createElement(SessionWorkspace, {
          ...baseProps(),
          events: [...first, event("assistant.message", { text: "new activity" })],
        }),
      );
    });

    // New content must not yank the viewport back down.
    expect(scroller.scrollTop).toBe(200);
    expect(screen.getByRole("button", { name: /Jump to latest/ })).toBeTruthy();
  });

  it("returns to the tail when the reader asks", async () => {
    const first = [event("user.message", { text: "one" })];
    const { rerender } = render(createElement(SessionWorkspace, { ...baseProps(), events: first }));
    const scroller = screen.getByTestId("activity-scroll");
    Object.defineProperty(scroller, "scrollHeight", { value: 2000, configurable: true });
    Object.defineProperty(scroller, "clientHeight", { value: 500, configurable: true });
    scroller.scrollTop = 100;
    fireEvent.scroll(scroller);

    await act(async () => {
      rerender(
        createElement(SessionWorkspace, { ...baseProps(), events: [...first, event("assistant.message", { text: "more" })] }),
      );
    });
    fireEvent.click(screen.getByRole("button", { name: /Jump to latest/ }));

    await waitFor(() => expect(scroller.scrollTop).toBe(2000));
    expect(screen.queryByRole("button", { name: /Jump to latest/ })).toBeNull();
  });
});

function baseProps(): SessionWorkspaceProps {
  return {
    events: [],
    hasConversation: true,
    approvals: [],
    connected: true,
    runPhase: "idle",
    running: false,
    projectName: "C:/repo",
    branch: "main",
    models: MODELS,
    modelCatalog: [],
    providerCredentials: [],
    isLoadingModelCatalog: false,
    permissions: PERMISSIONS,
    composer: {
      value: "",
      onChange: vi.fn(),
      onSubmit: vi.fn(),
      onCancel: vi.fn(),
      workspacePath: "C:/repo",
      onChooseWorkspace: vi.fn(),
      pendingMode: null,
      onSelectModel: vi.fn(),
      onRefreshModelCatalog: vi.fn(),
      onConnectProvider: vi.fn(),
      onSelectReasoning: vi.fn(),
      onSelectMode: vi.fn(),
    },
    onApprove: vi.fn(),
    onDeny: vi.fn(),
    onSelectFile: vi.fn(),
    inspector: { open: false, tab: "changes", onTabChange: vi.fn(), onToggle: vi.fn(), onClose: vi.fn() },
    changes: {
      entries: [],
      selectedPath: null,
      fileChange: null,
      fileView: null,
      isLoading: false,
      isTruncated: false,
      totalChanged: 0,
      isGitWorkspace: true,
    },
    onClearFile: vi.fn(),
    checkpoints: [],
    restoringId: null,
    lastRestore: null,
    restoreDisabled: false,
    onRestore: vi.fn(),
  };
}

describe("long sessions", () => {
  it("renders a long run without collapsing the composer out of reach", () => {
    const events: HarnessEvent[] = [event("user.message", { text: "go" })];
    for (let index = 0; index < 200; index += 1) {
      events.push(...toolCall("read_file", { path: `src/file-${index}.ts` }));
      events.push(event("tool.completed", { tool: "read_file" }));
    }
    setup({ events });
    expect(screen.getAllByTestId(/^tool-/).length).toBe(200);
    expect(screen.getByLabelText("Message the agent")).toBeTruthy();
  });

  it("does not render an empty state once the run has produced activity", () => {
    const blocks: ActivityBlock[] = buildActivityStream([event("assistant.message", { text: "hi" })]);
    expect(blocks).toHaveLength(1);
    setup({ events: [event("assistant.message", { text: "hi" })] });
    expect(screen.queryByText(/Send a task to start a durable session/)).toBeNull();
  });
});
