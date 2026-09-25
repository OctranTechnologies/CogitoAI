import { describe, expect, it } from "vitest";
import {
  checkpointTrigger,
  deriveCheckpoints,
  formatBytes,
  shouldRefreshChanges,
  summarizeChanges,
  type CheckpointInfo,
  type FileChange,
} from "./changes";
import type { HarnessEvent } from "./rpc";

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

function change(overrides: Partial<FileChange>): FileChange {
  return {
    path: "file.ts",
    language: "typescript",
    kind: "modified",
    original: "a\n",
    modified: "b\n",
    patch: "diff",
    additions: 1,
    deletions: 1,
    is_binary: false,
    truncated: false,
    ...overrides,
  };
}

function checkpoint(overrides: Partial<CheckpointInfo>): CheckpointInfo {
  return {
    id: "checkpoint-1",
    session_id: "session-1",
    working_directory: "/repo",
    created_at: "1700000000000",
    reference: "abc123",
    baseline_file_count: 12,
    recorded_changes: ["file.ts"],
    ...overrides,
  };
}

describe("summarizeChanges", () => {
  it("groups files into added, modified, and deleted buckets", () => {
    const summary = summarizeChanges([
      change({ path: "new.ts", kind: "added", additions: 4, deletions: 0 }),
      change({ path: "edited.ts", kind: "modified", additions: 3, deletions: 2 }),
      change({ path: "gone.ts", kind: "deleted", additions: 0, deletions: 9 }),
    ]);

    expect(summary.added.map((entry) => entry.path)).toEqual(["new.ts"]);
    expect(summary.modified.map((entry) => entry.path)).toEqual(["edited.ts"]);
    expect(summary.deleted.map((entry) => entry.path)).toEqual(["gone.ts"]);
    expect(summary.additions).toBe(7);
    expect(summary.deletions).toBe(11);
    expect(summary.entries).toHaveLength(3);
  });

  it("counts binary files separately and preserves their flag", () => {
    const summary = summarizeChanges([
      change({ path: "logo.png", is_binary: true, additions: 0, deletions: 0 }),
    ]);

    expect(summary.binaryCount).toBe(1);
    expect(summary.entries[0].isBinary).toBe(true);
  });

  it("returns an empty summary for no changes", () => {
    const summary = summarizeChanges([]);

    expect(summary.entries).toEqual([]);
    expect(summary.additions).toBe(0);
    expect(summary.deletions).toBe(0);
    expect(summary.binaryCount).toBe(0);
  });
});

describe("checkpoint derivation", () => {
  const events = [
    event("e1", "user.message", { text: "first task" }),
    event("e2", "checkpoint.created", { checkpoint_id: "checkpoint-1" }),
    event("e3", "user.message", { text: "second task" }),
    event("e4", "checkpoint.created", { checkpoint_id: "checkpoint-2" }),
  ];

  it("attributes each checkpoint to the task that triggered it", () => {
    expect(checkpointTrigger(events, "checkpoint-1")).toBe("first task");
    expect(checkpointTrigger(events, "checkpoint-2")).toBe("second task");
  });

  it("builds a timeline with id, time, trigger, and affected files", () => {
    const entries = deriveCheckpoints(
      [
        checkpoint({ id: "checkpoint-1", recorded_changes: ["a.ts", "b.ts"], created_at: "1700000000000" }),
        checkpoint({ id: "checkpoint-2", created_at: "1700000001000" }),
      ],
      events,
    );

    expect(entries).toHaveLength(2);
    expect(entries[0]).toMatchObject({
      id: "checkpoint-1",
      createdAt: 1700000000000,
      trigger: "first task",
      affectedFiles: ["a.ts", "b.ts"],
      isRestored: false,
    });
    expect(entries[1]).toMatchObject({ id: "checkpoint-2", trigger: "second task" });
  });

  it("marks a checkpoint as restored from the runtime restore event", () => {
    const entries = deriveCheckpoints(
      [checkpoint({ id: "checkpoint-1" })],
      [
        ...events,
        event("e5", "checkpoint.restored", {
          checkpoint_id: "checkpoint-1",
          restored_files: ["a.ts", "b.ts"],
        }),
      ],
    );

    expect(entries[0].isRestored).toBe(true);
    expect(entries[0].restoredFiles).toEqual(["a.ts", "b.ts"]);
  });

  it("falls back to an unknown timestamp when the value is not a number", () => {
    const entries = deriveCheckpoints([checkpoint({ created_at: "not-a-number" })], events);

    expect(entries[0].createdAt).toBe(0);
  });
});

describe("shouldRefreshChanges", () => {
  it("refreshes on file and checkpoint events", () => {
    expect(shouldRefreshChanges(event("a", "file.changed", {}))).toBe(true);
    expect(shouldRefreshChanges(event("b", "checkpoint.created", {}))).toBe(true);
    expect(shouldRefreshChanges(event("c", "checkpoint.restored", {}))).toBe(true);
  });

  it("refreshes when a mutating tool completes", () => {
    expect(shouldRefreshChanges(event("d", "tool.completed", { tool: "write_file" }))).toBe(true);
    expect(shouldRefreshChanges(event("e", "tool.completed", { tool: "apply_patch" }))).toBe(true);
  });

  it("does not refresh for read-only tools or chat events", () => {
    expect(shouldRefreshChanges(event("f", "tool.completed", { tool: "list_directory" }))).toBe(false);
    expect(shouldRefreshChanges(event("g", "assistant.delta", { text: "hi" }))).toBe(false);
    expect(shouldRefreshChanges(event("h", "user.message", { text: "hi" }))).toBe(false);
  });

  it("refreshes when a run ends so late changes are picked up", () => {
    expect(shouldRefreshChanges(event("i", "session.completed", {}))).toBe(true);
    expect(shouldRefreshChanges(event("j", "session.failed", {}))).toBe(true);
  });
});

describe("formatBytes", () => {
  it("formats sizes for display", () => {
    expect(formatBytes(512)).toBe("512 B");
    expect(formatBytes(2048)).toBe("2.0 KB");
    expect(formatBytes(5 * 1024 * 1024)).toBe("5.0 MB");
  });
});
