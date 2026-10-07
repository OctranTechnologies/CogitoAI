import { describe, expect, it } from "vitest";
import {
  buildActivityStream,
  describeTool,
  summariseStream,
  visibleArguments,
  type ToolBlock,
} from "./activity";
import type { HarnessEvent } from "./rpc";

/**
 * Fixtures use the runtime's real event names and payload keys, so a rename on
 * the Rust side fails these tests rather than quietly emptying the workspace.
 */
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

describe("describeTool", () => {
  it("phrases the harness's own tools the way a person would say them", () => {
    expect(describeTool("read_file", { path: "src/auth.ts" })).toBe("Reading src/auth.ts");
    expect(describeTool("write_file", { path: "src/session.ts", content: "x" })).toBe("Writing src/session.ts");
    expect(describeTool("apply_patch", { path: "src/session.ts", old_text: "a", new_text: "b" })).toBe(
      "Editing src/session.ts",
    );
    expect(describeTool("list_directory", { path: "src/lib" })).toBe("Listing src/lib");
    expect(describeTool("glob", { pattern: "**/*.test.ts" })).toBe("Finding **/*.test.ts");
    expect(describeTool("grep", { pattern: "refreshToken" })).toBe("Searching refreshToken");
    expect(describeTool("shell", { command: "pnpm test" })).toBe("Running pnpm test");
  });

  it("does not invent a category for a tool it has never heard of", () => {
    expect(describeTool("some_future_tool", { path: "src/x.ts" })).toBe("some_future_tool src/x.ts");
    expect(describeTool("some_future_tool", {})).toBe("some_future_tool");
  });

  it("shortens a long command rather than wrapping the row", () => {
    const long = "pnpm run build && pnpm run test && pnpm run lint && pnpm run format";
    const label = describeTool("shell", { command: long });
    expect(label.startsWith("Running ")).toBe(true);
    expect(label.length).toBeLessThanOrEqual("Running ".length + 60);
    expect(label.endsWith("...")).toBe(true);
  });

  it("shows bounded delegation compactly and hides child context payloads", () => {
    expect(
      describeTool("delegate_subagents", {
        tasks: JSON.stringify({ tasks: [{ role: "explore" }, { role: "review" }] }),
      }),
    ).toBe("Delegating to 2 read-only agents");
    expect(
      visibleArguments({ tasks: "large task payload", selected_context: "source text", role: "review" }),
    ).toEqual({ role: "review" });
  });

  it("keeps the row phrased when a required argument is missing", () => {
    expect(describeTool("read_file", {})).toBe("Reading a file");
    expect(describeTool("grep", {})).toBe("Searching");
    expect(describeTool("shell", {})).toBe("Running a command");
  });
});

describe("visibleArguments", () => {
  it("hides bulky file bodies from the expanded row", () => {
    const shown = visibleArguments({
      path: "src/session.ts",
      content: "a very long file body",
      old_text: "before",
      new_text: "after",
    });
    expect(Object.keys(shown)).toEqual(["path"]);
  });
});

describe("buildActivityStream", () => {
  it("keeps background process status compact and updates it when the process exits", () => {
    const blocks = buildActivityStream([
      event("background_process.started", {
        process_id: "proc-123-0",
        command: "npm run dev",
        working_directory: "C:/repo",
        pid: 123,
        started_at_unix_ms: 1,
      }),
      event("background_process.status", {
        process_id: "proc-123-0",
        pid: 123,
        status: "exited",
        exit_code: 0,
        timed_out: false,
      }),
    ]);

    expect(blocks).toHaveLength(1);
    expect(blocks[0]).toMatchObject({
      kind: "notice",
      label: "Background process exited",
      detail: "proc-123-0 · npm run dev · PID 123 · exited (0)",
      tone: "success",
      reference: "proc-123-0",
    });
  });

  it("orders a run the way the runtime reported it", () => {
    const blocks = buildActivityStream([
      event("user.message", { text: "fix the parser" }),
      ...toolCall("read_file", { path: "src/auth.ts" }),
      event("tool.completed", { tool: "read_file" }),
      event("assistant.message", { text: "Parsed it." }),
      event("session.completed", { reason: "done" }),
    ]);

    expect(blocks.map((block) => block.kind)).toEqual([
      "user",
      "tool",
      "assistant",
      "notice",
    ]);
  });

  it("folds streamed deltas into one message that grows in place", () => {
    const blocks = buildActivityStream([
      event("user.message", { text: "hello" }),
      event("assistant.delta", { text: "Hel" }),
      event("assistant.delta", { text: "lo " }),
      event("assistant.delta", { text: "there" }),
    ]);

    const assistant = blocks.filter((block) => block.kind === "assistant");
    expect(assistant).toHaveLength(1);
    expect(assistant[0]).toMatchObject({ text: "Hello there", streaming: true });
  });

  it("closes a streamed turn with the authoritative message instead of duplicating it", () => {
    const blocks = buildActivityStream([
      event("assistant.delta", { text: "partial" }),
      event("assistant.message", { text: "the complete answer" }),
    ]);
    const assistant = blocks.filter((block) => block.kind === "assistant");
    expect(assistant).toHaveLength(1);
    expect(assistant[0]).toMatchObject({ text: "the complete answer", streaming: false });
  });

  it("attaches the model and token counts the runtime reported", () => {
    const blocks = buildActivityStream([
      event("assistant.message", { text: "hi" }),
      event("model.response", { provider: "openai", model: "gpt-4o", input_tokens: 120, output_tokens: 34 }),
    ]);
    const assistant = blocks.find((block) => block.kind === "assistant");
    expect(assistant).toMatchObject({
      provider: "openai",
      model: "gpt-4o",
      inputTokens: 120,
      outputTokens: 34,
    });
  });

  it("renders model metadata for provider IDs unknown to the desktop", () => {
    const blocks = buildActivityStream([
      event("assistant.message", { text: "response" }),
      event("model.response", {
        provider: "future-provider",
        model: "future-provider/example-model",
        input_tokens: 3,
        output_tokens: 2,
      }),
    ]);
    expect(blocks.find((block) => block.kind === "assistant")).toMatchObject({
      provider: "future-provider",
      model: "future-provider/example-model",
      inputTokens: 3,
      outputTokens: 2,
    });
  });

  it("tracks a tool call through to its outcome with a duration", () => {
    const requested = event("tool.requested", { tool: "shell", arguments: { command: "pnpm test" } });
    const started = event("tool.started", { tool: "shell" }, requested.timestamp + 100);
    const completed = event("tool.completed", { tool: "shell" }, started.timestamp + 1500);

    const blocks = buildActivityStream([requested, started, completed]);
    const tool = blocks[0] as ToolBlock;
    expect(tool).toMatchObject({ label: "Running pnpm test", phase: "succeeded", tone: "success" });
    expect(tool.durationMs).toBe(1500);
  });

  it("marks a failed call as failed and keeps the error", () => {
    const blocks = buildActivityStream([
      ...toolCall("shell", { command: "pnpm test" }),
      event("tool.failed", { tool: "shell", error: "exit code 1" }),
    ]);
    expect(blocks[0]).toMatchObject({ phase: "failed", tone: "failure", error: "exit code 1" });
  });

  it("keeps concurrent calls to the same tool distinct", () => {
    const blocks = buildActivityStream([
      event("tool.requested", { tool: "read_file", arguments: { path: "a.ts" } }),
      event("tool.requested", { tool: "read_file", arguments: { path: "b.ts" } }),
      event("tool.started", { tool: "read_file" }),
      event("tool.completed", { tool: "read_file" }),
    ]);
    const tools = blocks.filter((block): block is ToolBlock => block.kind === "tool");
    expect(tools.map((tool) => tool.path)).toEqual(["a.ts", "b.ts"]);
    // The completed event closes the most recent open call, not the first.
    expect(tools[1].phase).toBe("succeeded");
    expect(tools[0].phase).toBe("requested");
  });

  it("summarises verification as the run's headline outcome", () => {
    const started = event("verification.started", { commands: ["pnpm test", "cargo test"] });
    const blocks = buildActivityStream([
      started,
      event("verification.result", {
        command: "pnpm test",
        category: "test",
        passed: true,
        duration_ms: 4200,
        exit_code: 0,
        output: "",
        diagnostics: ["suite a", "suite b", "suite c", "suite d"],
      }),
      event("verification.result", {
        command: "cargo test",
        category: "test",
        passed: false,
        duration_ms: 900,
        exit_code: 101,
        output: "",
        diagnostics: [],
      }),
    ]);

    const checks = blocks.filter((block) => block.kind === "verification");
    expect(checks).toHaveLength(2);
    expect(checks[0]).toMatchObject({ passed: true, tone: "success", durationMs: 4200 });
    expect(checks[1]).toMatchObject({ passed: false, tone: "failure", exitCode: 101 });
  });

  it("surfaces a failed run as an error block", () => {
    const blocks = buildActivityStream([event("session.failed", { error: "model unavailable" })]);
    expect(blocks[0]).toMatchObject({ kind: "error", detail: "model unavailable" });
  });

  it("reports checkpoints, compaction, and resume without cluttering the stream", () => {
    const blocks = buildActivityStream([
      event("session.resumed", { reason: "operator" }),
      event("checkpoint.created", { checkpoint_id: "c1", reference: "abc1234" }),
      event("context.compacted", { removed_items: 3, summary: "…", state: {} }),
      event("checkpoint.restored", { checkpoint_id: "c1", restored_files: ["a.ts"], conflicts: [] }),
    ]);
    expect(blocks.map((block) => block.kind)).toEqual(["notice", "notice", "notice", "notice"]);
    const notices = blocks as { label: string; detail: string; tone: string; reference: string | null }[];
    expect(notices[1]).toMatchObject({ label: "Checkpoint", reference: "abc1234" });
    expect(notices[2].detail).toBe("3 items summarised");
    expect(notices[3].detail).toBe("1 file restored");
  });

  it("flags a restore that hit conflicts", () => {
    const blocks = buildActivityStream([
      event("checkpoint.restored", {
        checkpoint_id: "c1",
        restored_files: [],
        conflicts: ["src/a.ts"],
      }),
    ]);
    expect(blocks[0]).toMatchObject({ kind: "notice", tone: "attention" });
    expect((blocks[0] as { detail: string }).detail).toBe("src/a.ts");
  });

  it("leaves file changes and policy decisions to the inspector", () => {
    const blocks = buildActivityStream([
      event("file.changed", { path: "src/a.ts", change: "modified" }),
      event("policy.decision", { tool: "shell", action: "allow", reason: "safe", rule: "r", operation: "o", mode: "safe" }),
    ]);
    expect(blocks).toEqual([]);
  });
});

describe("approvals", () => {
  it("attaches a pending approval to its tool row instead of duplicating it", () => {
    const blocks = buildActivityStream(
      [event("tool.requested", { tool: "shell", arguments: { command: "rm -rf build" } })],
      { approvals: [{ approval_id: "ap1", tool: { name: "shell", arguments: { command: "rm -rf build" } } }] },
    );

    expect(blocks).toHaveLength(1);
    expect(blocks[0]).toMatchObject({
      kind: "tool",
      phase: "awaiting_approval",
      tone: "attention",
      approvalId: "ap1",
    });
  });

  it("prefers the arguments the approval notification carried", () => {
    const blocks = buildActivityStream(
      [event("tool.requested", { tool: "write_file", arguments: {} })],
      { approvals: [{ approval_id: "ap1", tool: { name: "write_file", arguments: { path: "src/x.ts" } } }] },
    );
    expect(blocks[0]).toMatchObject({ label: "Writing src/x.ts", path: "src/x.ts" });
  });

  it("resumes the call once it is approved", () => {
    const blocks = buildActivityStream([
      event("tool.requested", { tool: "shell", arguments: { command: "pnpm test" } }),
      event("tool.approved", { tool: "shell", reason: null }),
    ]);
    expect(blocks[0]).toMatchObject({ phase: "running", tone: "running" });
  });

  it("records a denial as a failure the person can read", () => {
    const blocks = buildActivityStream([
      event("tool.requested", { tool: "shell", arguments: { command: "rm -rf /" } }),
      event("tool.denied", { tool: "shell", reason: "not allowed" }),
    ]);
    expect(blocks[0]).toMatchObject({ phase: "denied", tone: "failure", error: "not allowed" });
  });

  it("still shows an approval whose tool call has scrolled out of the log", () => {
    const blocks = buildActivityStream([], {
      approvals: [{ approval_id: "ap9", tool: { name: "shell", arguments: { command: "ls" } } }],
    });
    expect(blocks).toHaveLength(1);
    expect(blocks[0]).toMatchObject({ kind: "approval", approvalId: "ap9" });
  });
});

describe("summariseStream", () => {
  it("counts outcomes for the header", () => {
    const blocks = buildActivityStream([
      ...toolCall("read_file", { path: "a.ts" }),
      event("tool.completed", { tool: "read_file" }),
      ...toolCall("grep", { pattern: "x" }),
      event("tool.failed", { tool: "grep", error: "no match" }),
    ]);
    const summary = summariseStream(blocks);
    expect(summary.tools).toBe(2);
    expect(summary.failures).toBe(1);
    expect(summary.awaitingApproval).toBe(0);
  });
});

describe("long sessions", () => {
  it("stays linear and ordered for a long log", () => {
    const events: HarnessEvent[] = [event("user.message", { text: "go" })];
    for (let index = 0; index < 500; index += 1) {
      events.push(...toolCall("read_file", { path: `src/file-${index}.ts` }));
      events.push(event("tool.completed", { tool: "read_file" }));
    }
    const blocks = buildActivityStream(events);
    expect(blocks).toHaveLength(501);
    const tools = blocks.filter((block): block is ToolBlock => block.kind === "tool");
    expect(tools[0].path).toBe("src/file-0.ts");
    expect(tools[499].path).toBe("src/file-499.ts");
  });

  it("caps a runaway tool output rather than holding it all in memory", () => {
    const blocks = buildActivityStream([
      event("tool.requested", { tool: "shell", arguments: { command: "cat big.log" } }),
      event("tool.output", { tool: "shell", output: "x".repeat(50_000) }),
    ]);
    expect((blocks[0] as ToolBlock).output.length).toBe(8000);
  });
});
