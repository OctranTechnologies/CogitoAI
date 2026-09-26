import { describe, expect, it } from "vitest";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { renderToString } from "react-dom/server";
import { createElement } from "react";
import { buildActivityStream, summariseStream, type ToolBlock } from "./activity";
import { ActivityStream } from "../components/activity-stream";
import type { HarnessEvent } from "./rpc";

/**
 * A recording of a real run, captured by `scripts/live-session.mjs` driving the
 * scripted mock runtime over the loopback protocol.
 *
 * The hand-written fixtures elsewhere prove the derivation is self-consistent.
 * This one proves it matches what the runtime actually emits — the event names,
 * the payload keys, the ordering, and the fact that streamed text arrives as
 * deltas before a final message. Re-record it with:
 *
 *   cargo build -p harness-rpc --bin cogito-rpc-dev
 *   node scripts/live-session.mjs
 */
const here = dirname(fileURLToPath(import.meta.url));
const events = JSON.parse(
  readFileSync(join(here, "live-session.fixture.json"), "utf8"),
) as HarnessEvent[];

describe("recorded runtime session", () => {
  it("contains the event kinds the workspace is built around", () => {
    const types = new Set(events.map((event) => event.event_type));
    for (const expected of [
      "session.started",
      "user.message",
      "model.requested",
      "tool.requested",
      "policy.decision",
      "tool.approved",
      "tool.started",
      "tool.output",
      "tool.completed",
      "file.changed",
      "checkpoint.created",
      "assistant.delta",
      "assistant.message",
      "session.completed",
    ]) {
      expect(types, `expected the runtime to emit ${expected}`).toContain(expected);
    }
  });

  it("derives an ordered stream in the order the runtime reported it", () => {
    const blocks = buildActivityStream(events);
    const kinds = blocks.map((block) => block.kind);

    expect(kinds).toContain("user");
    expect(kinds).toContain("tool");
    expect(kinds).toContain("assistant");
    expect(kinds[kinds.length - 1]).toBe("notice");

    // Nothing may be reordered relative to the event log: the task, then tool
    // work, then the reply that came out of it.
    const firstUser = kinds.indexOf("user");
    const firstTool = kinds.indexOf("tool");
    const firstAssistant = kinds.indexOf("assistant");
    expect(firstUser).toBeLessThan(firstTool);
    expect(firstTool).toBeLessThan(firstAssistant);
  });

  it("phrases the tools the runtime actually called", () => {
    const tools = buildActivityStream(events).filter(
      (block): block is ToolBlock => block.kind === "tool",
    );
    expect(tools.length).toBeGreaterThan(0);
    // The scripted mock calls list_directory then write_file. The arguments are
    // recorded unquoted, so the labels read as a person would say them.
    expect(tools[0].label).toBe("Listing .");
    expect(tools[1].label).toBe("Writing cogito-rpc-dev-output.txt");
    for (const tool of tools) {
      expect(tool.label, "no label should carry a stray quote").not.toContain('"');
      expect(tool.phase, `${tool.label} should be settled`).toBe("succeeded");
      expect(tool.tone).toBe("success");
    }
  });

  it("folds the recorded deltas into a single assistant message", () => {
    const deltas = events.filter((event) => event.event_type === "assistant.delta");
    expect(deltas.length).toBeGreaterThan(1);

    const assistants = buildActivityStream(events).filter((block) => block.kind === "assistant");
    expect(assistants).toHaveLength(1);
    expect(assistants[0].streaming).toBe(false);
    expect(assistants[0].text.length).toBeGreaterThan(0);
  });

  it("records that the policy approved the calls the runtime made", () => {
    const approvals = events.filter((event) => event.event_type === "tool.approved");
    expect(approvals.length).toBeGreaterThan(0);
    const decisions = events.filter((event) => event.event_type === "policy.decision");
    expect(decisions.length).toBeGreaterThan(0);
  });

  it("summarises a clean run with no failures", () => {
    const summary = summariseStream(buildActivityStream(events));
    expect(summary.tools).toBeGreaterThan(0);
    expect(summary.failures).toBe(0);
    expect(summary.awaitingApproval).toBe(0);
  });

  it("renders the recorded run to markup with every row present", () => {
    const blocks = buildActivityStream(events);
    const html = renderToString(
      createElement(ActivityStream, {
        blocks,
        onApprove: () => {},
        onDeny: () => {},
        onOpenFile: () => {},
      }),
    );

    expect(html).toContain("activity-stream");
    for (const tool of blocks.filter((block): block is ToolBlock => block.kind === "tool")) {
      expect(html).toContain(tool.label);
    }
    // A raw palette value must never reach the markup.
    expect(html).not.toMatch(/style="[^"]*#[0-9a-f]{6}/i);
  });
});
