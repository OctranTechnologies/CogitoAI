import { describe, expect, it } from "vitest";
import type { HarnessEvent } from "./rpc";
import {
  deriveTimeline,
  deriveToolActivities,
  deriveVerificationActivities,
  hydrateConversation,
} from "./events";

function event(id: string, type: string, data: Record<string, unknown>, timestamp = id.length): HarnessEvent {
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

describe("runtime event projections", () => {
  it("derives complete tool activity from runtime events", () => {
    const events = [
      event("a", "tool.requested", { tool: "write_file", arguments: { path: "src/lib.rs", content: "x" } }, 10),
      event("b", "tool.approved", { tool: "write_file", reason: "ask" }, 11),
      event("c", "tool.started", { tool: "write_file" }, 12),
      event("d", "tool.output", { tool: "write_file", output: "wrote src/lib.rs" }, 13),
      event("e", "tool.completed", { tool: "write_file" }, 42),
    ];

    const [tool] = deriveToolActivities(events);

    expect(tool).toMatchObject({ name: "write_file", target: "src/lib.rs", state: "succeeded", durationMs: 30, output: "wrote src/lib.rs" });
  });

  it("derives tool failure and denial states", () => {
    const failed = deriveToolActivities([
      event("a", "tool.requested", { tool: "shell", arguments: { command: "false" } }, 1),
      event("b", "tool.started", { tool: "shell" }, 2),
      event("c", "tool.failed", { tool: "shell", error: "exit 1" }, 5),
    ])[0];
    const denied = deriveToolActivities([
      event("d", "tool.requested", { tool: "write_file", arguments: { path: "blocked" } }, 1),
      event("e", "tool.denied", { tool: "write_file", reason: "user denied" }, 2),
    ])[0];
    expect(failed).toMatchObject({ state: "failed", durationMs: 3, error: "exit 1" });
    expect(denied).toMatchObject({ state: "denied", error: "user denied" });
  });

  it("derives verification results and diagnostics", () => {
    const activities = deriveVerificationActivities([
      event("a", "verification.started", { commands: ["cargo test"] }, 1),
      event("b", "verification.result", { command: "cargo test", category: "GeneralTest", passed: false, exit_code: 1, duration_ms: 90, diagnostics: ["test failed"] }, 2),
    ]);
    expect(activities[0]).toMatchObject({ command: "cargo test", category: "GeneralTest", state: "failed", exitCode: 1, durationMs: 90, diagnostics: ["test failed"] });
  });

  it("hydrates persisted conversation and chronological timeline", () => {
    const events = [
      event("a", "user.message", { text: "Fix the test" }, 1),
      event("b", "tool.completed", { tool: "read_file" }, 2),
      event("c", "assistant.message", { text: "Fixed" }, 3),
      event("d", "session.completed", { reason: null }, 4),
    ];
    expect(hydrateConversation(events).map((message) => [message.role, message.text])).toEqual([
      ["user", "Fix the test"],
      ["assistant", "Fixed"],
    ]);
    expect(deriveTimeline(events).map((entry) => entry.eventType)).toEqual([
      "user.message",
      "tool.completed",
      "assistant.message",
      "session.completed",
    ]);
  });
});
