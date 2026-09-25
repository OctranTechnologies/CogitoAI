import type { HarnessEvent } from "./rpc";

export type FileChangeKind = "added" | "modified" | "deleted";

export interface GitStatusSummary {
  repository_root: string;
  branch: string | null;
  head: string | null;
  is_clean: boolean;
  changed_files: string[];
  staged_files: string[];
  unstaged_files: string[];
  untracked_files: string[];
}

export interface FileChange {
  path: string;
  language: string;
  kind: FileChangeKind;
  original: string;
  modified: string;
  patch: string;
  additions: number;
  deletions: number;
  is_binary: boolean;
  truncated: boolean;
}

export interface FileView {
  path: string;
  language: string;
  content: string;
  size_bytes: number;
  is_binary: boolean;
  truncated: boolean;
}

export interface CheckpointInfo {
  id: string;
  session_id: string;
  working_directory: string;
  created_at: string;
  reference: string;
  baseline_file_count: number;
  recorded_changes: string[];
}

export interface RestoreReport {
  checkpoint_id: string;
  restored_files: string[];
  conflicts: string[];
}

/** A changed file enriched with the line counts needed for the file list. */
export interface ChangeEntry {
  path: string;
  kind: FileChangeKind;
  additions: number;
  deletions: number;
  isBinary: boolean;
  truncated: boolean;
  language: string;
}

export interface ChangeSummary {
  entries: ChangeEntry[];
  added: ChangeEntry[];
  modified: ChangeEntry[];
  deleted: ChangeEntry[];
  additions: number;
  deletions: number;
  binaryCount: number;
}

/** A checkpoint with its triggering run inferred from runtime events. */
export interface CheckpointEntry {
  id: string;
  createdAt: number;
  reference: string;
  trigger: string;
  affectedFiles: string[];
  sessionId: string;
  isRestored: boolean;
  restoredFiles: string[];
}

const CHANGE_EVENTS = new Set(["file.changed", "checkpoint.created", "checkpoint.restored"]);

function text(event: HarnessEvent): string {
  return typeof event.payload.data.text === "string" ? event.payload.data.text : "";
}

function stringList(event: HarnessEvent, key: string): string[] {
  const value = event.payload.data[key];
  return Array.isArray(value) ? value.filter((item): item is string => typeof item === "string") : [];
}

/**
 * Derives a checkpoint's triggering action from the runtime event stream.
 *
 * The runtime records `checkpoint.created` when a run opens a checkpoint. The
 * nearest preceding `user.message` is the task that triggered it, so the
 * timeline can explain *why* a checkpoint exists without the frontend guessing.
 */
export function checkpointTrigger(events: HarnessEvent[], checkpointId: string): string {
  let trigger = "";
  for (const event of events) {
    if (event.event_type === "checkpoint.created") {
      const id = event.payload.data.checkpoint_id;
      if (id === checkpointId) break;
      continue;
    }
    if (event.event_type === "user.message") {
      const value = text(event).trim();
      if (value) trigger = value;
    }
  }
  return trigger;
}

/**
 * Builds the checkpoint timeline from runtime checkpoint records plus the
 * durable event stream. Restore outcomes are read from `checkpoint.restored`
 * events so the UI reflects what the runtime actually did.
 */
export function deriveCheckpoints(
  checkpoints: CheckpointInfo[],
  events: HarnessEvent[],
): CheckpointEntry[] {
  const restores = new Map<string, string[]>();
  for (const event of events) {
    if (event.event_type !== "checkpoint.restored") continue;
    const id = event.payload.data.checkpoint_id;
    if (typeof id === "string") restores.set(id, stringList(event, "restored_files"));
  }
  return checkpoints.map((checkpoint) => {
    const restoredFiles = restores.get(checkpoint.id);
    const createdAt = Number.parseInt(checkpoint.created_at, 10);
    return {
      id: checkpoint.id,
      createdAt: Number.isFinite(createdAt) ? createdAt : 0,
      reference: checkpoint.reference,
      trigger: checkpointTrigger(events, checkpoint.id),
      affectedFiles: checkpoint.recorded_changes,
      sessionId: checkpoint.session_id,
      isRestored: restoredFiles !== undefined,
      restoredFiles: restoredFiles ?? [],
    };
  });
}

/** Groups a flat change list into the added/modified/deleted buckets the panel renders. */
export function summarizeChanges(changes: FileChange[]): ChangeSummary {
  const entries: ChangeEntry[] = changes.map((change) => ({
    path: change.path,
    kind: change.kind,
    additions: change.additions,
    deletions: change.deletions,
    isBinary: change.is_binary,
    truncated: change.truncated,
    language: change.language,
  }));
  const added = entries.filter((entry) => entry.kind === "added");
  const modified = entries.filter((entry) => entry.kind === "modified");
  const deleted = entries.filter((entry) => entry.kind === "deleted");
  return {
    entries,
    added,
    modified,
    deleted,
    additions: entries.reduce((total, entry) => total + entry.additions, 0),
    deletions: entries.reduce((total, entry) => total + entry.deletions, 0),
    binaryCount: entries.filter((entry) => entry.isBinary).length,
  };
}

/**
 * Determines whether a runtime event should trigger a refresh of the code
 * change view. The desktop never polls; it re-reads git and checkpoint state
 * from the runtime only when the runtime says something changed on disk.
 */
export function shouldRefreshChanges(event: HarnessEvent): boolean {
  if (CHANGE_EVENTS.has(event.event_type)) return true;
  if (event.event_type === "tool.completed") {
    const tool = event.payload.data.tool;
    return typeof tool === "string" && /write|edit|patch|delete|move|rename/i.test(tool);
  }
  return event.event_type === "session.completed" || event.event_type === "session.failed";
}

export function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

export function formatTimestamp(milliseconds: number): string {
  if (!milliseconds) return "unknown time";
  return new Date(milliseconds).toLocaleString();
}
