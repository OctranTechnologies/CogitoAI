import type { ApprovalRequest, HarnessEvent } from "./rpc";

/**
 * The conversation/activity workspace is derived from the runtime event log and
 * nothing else.
 *
 * The store keeps a parallel `messages` array, which is fine for "what did the
 * user say" but cannot answer "what is the agent doing right now" without
 * interleaving a second source. Deriving one ordered stream from events means
 * ordering, streaming text, tool state, approvals, and failures can never
 * disagree with each other, and a reconnected or resumed session rebuilds
 * itself from the same log.
 *
 * Nothing here reaches for state that the runtime did not send. If the runtime
 * does not report a branch, the UI shows no branch.
 */

/** Lifecycle of one tool call, as reported by the runtime. */
export type ToolPhase =
  | "requested"
  | "awaiting_approval"
  | "running"
  | "succeeded"
  | "failed"
  | "denied";

/** The four indicators the workspace distinguishes, deliberately restrained. */
export type ActivityTone = "running" | "success" | "failure" | "attention" | "neutral";

export interface UserBlock {
  kind: "user";
  id: string;
  text: string;
  timestamp: number;
}

export interface AssistantBlock {
  kind: "assistant";
  id: string;
  text: string;
  timestamp: number;
  /** True while deltas are still arriving for this message. */
  streaming: boolean;
  provider: string | null;
  model: string | null;
  inputTokens: number | null;
  outputTokens: number | null;
}

export interface ToolBlock {
  kind: "tool";
  id: string;
  tool: string;
  /** Compact, human phrasing such as "Reading src/auth.ts". */
  label: string;
  /** The raw arguments, shown only when the row is expanded. */
  arguments: Record<string, unknown>;
  phase: ToolPhase;
  tone: ActivityTone;
  timestamp: number;
  durationMs: number | null;
  output: string;
  error: string;
  /** Set when the row can be opened in the inspector. */
  path: string | null;
  /** Populated while the call is waiting on a human decision. */
  approvalId: string | null;
  approvalReason: string | null;
  approvalRisks: string[];
}

export interface VerificationBlock {
  kind: "verification";
  id: string;
  command: string;
  category: string;
  passed: boolean | null;
  tone: ActivityTone;
  durationMs: number | null;
  exitCode: number | null;
  diagnostics: string[];
  output: string;
}

export interface ApprovalBlock {
  kind: "approval";
  id: string;
  approvalId: string;
  tool: string;
  arguments: Record<string, unknown>;
  timestamp: number;
  reason: string;
  risks: string[];
}

export interface ErrorBlock {
  kind: "error";
  id: string;
  title: string;
  detail: string;
  timestamp: number;
}

export interface NoticeBlock {
  kind: "notice";
  id: string;
  label: string;
  detail: string;
  timestamp: number;
  tone: ActivityTone;
  /** Checkpoint reference, when the notice is a checkpoint event. */
  reference: string | null;
}

export type ActivityBlock =
  | UserBlock
  | AssistantBlock
  | ToolBlock
  | VerificationBlock
  | ApprovalBlock
  | ErrorBlock
  | NoticeBlock;

/** Arguments worth showing when a tool row is expanded. */
const ARGUMENT_HIDDEN = new Set([
  "content",
  "old_text",
  "new_text",
  "patch",
  "tasks",
  "selected_context",
]);

/** Maximum characters of a tool's output kept for the expanded row. */
const OUTPUT_LIMIT = 8000;

function text(event: HarnessEvent): string {
  return typeof event.payload.data.text === "string" ? event.payload.data.text : "";
}


function stringField(event: HarnessEvent, key: string): string {
  const value = event.payload.data[key];
  return typeof value === "string" ? value : "";
}

function numberField(event: HarnessEvent, key: string): number | null {
  const value = event.payload.data[key];
  return typeof value === "number" ? value : null;
}

function stringList(event: HarnessEvent, key: string): string[] {
  const value = event.payload.data[key];
  return Array.isArray(value) ? value.filter((item): item is string => typeof item === "string") : [];
}

function recordField(event: HarnessEvent, key: string): Record<string, unknown> {
  const value = event.payload.data[key];
  return value && typeof value === "object" && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : {};
}

function firstString(record: Record<string, unknown>, keys: string[]): string | null {
  for (const key of keys) {
    const value = record[key];
    if (typeof value === "string" && value.trim()) return value;
  }
  return null;
}

/** Shortens a long command to its first meaningful token, for the collapsed row. */
function commandLabel(command: string): string {
  const collapsed = command.replace(/\s+/g, " ").trim();
  if (collapsed.length <= 60) return collapsed;
  return `${collapsed.slice(0, 57)}...`;
}

/**
 * Maps a runtime tool name and its arguments to one compact phrase.
 *
 * The tool names and argument keys are the harness's, not invented here:
 * `read_file(path)`, `write_file(path, content)`, `apply_patch(path, old_text,
 * new_text)`, `list_directory(path)`, `glob(pattern, path)`, `grep(pattern,
 * path)`, and `shell(command, working_directory)`. An unknown tool falls back
 * to its own name rather than guessing at a category.
 */
export function describeTool(tool: string, args: Record<string, unknown>): string {
  const path = firstString(args, ["path", "file", "file_path"]);
  const command = firstString(args, ["command"]);
  const pattern = firstString(args, ["pattern", "query", "pattern_or_glob"]);
  const detail = path ?? command ?? pattern;

  switch (tool) {
    case "read_file":
      return detail ? `Reading ${detail}` : "Reading a file";
    case "write_file":
      return detail ? `Writing ${detail}` : "Writing a file";
    case "apply_patch":
      return detail ? `Editing ${detail}` : "Editing a file";
    case "list_directory":
      return detail ? `Listing ${detail}` : "Listing the workspace";
    case "glob":
      return pattern ? `Finding ${pattern}` : "Finding files";
    case "grep":
      return pattern ? `Searching ${pattern}` : "Searching";
    case "shell":
    case "run_command":
      return command ? `Running ${commandLabel(command)}` : "Running a command";
    case "start_background_command":
      return command ? `Starting ${commandLabel(command)}` : "Starting a background command";
    case "read_process_output":
      return "Reading process logs";
    case "list_processes":
      return "Inspecting processes";
    case "stop_process":
      return "Stopping a process";
    case "wait_for_process_output":
      return "Waiting for process readiness";
    case "delegate_subagents": {
      let tasks = Array.isArray(args.tasks) ? args.tasks.length : 1;
      if (typeof args.tasks === "string") {
        try {
          const parsed: unknown = JSON.parse(args.tasks);
          if (parsed && typeof parsed === "object" && "length" in parsed) {
            const value = (parsed as { length?: unknown }).length;
            if (typeof value === "number") tasks = value;
          } else if (parsed && typeof parsed === "object" && "tasks" in parsed) {
            const value = (parsed as { tasks?: unknown }).tasks;
            if (Array.isArray(value)) tasks = value.length;
          }
        } catch {
          // Event payloads from older runtime builds may contain a truncated
          // argument string; keep the compact single-agent fallback.
        }
      }
      return `Delegating to ${tasks} read-only agent${tasks === 1 ? "" : "s"}`;
    }
    default:
      return detail ? `${tool} ${detail}` : tool;
  }
}

/** The path a tool row can hand to the inspector, if it names a file. */
function toolPath(tool: string, args: Record<string, unknown>): string | null {
  return tool === "read_file" || tool === "write_file" || tool === "apply_patch"
    ? firstString(args, ["path", "file", "file_path"])
    : null;
}

/** Arguments worth showing when expanded: everything except bulky file bodies. */
export function visibleArguments(args: Record<string, unknown>): Record<string, unknown> {
  const kept: Record<string, unknown> = {};
  for (const [key, value] of Object.entries(args)) {
    if (ARGUMENT_HIDDEN.has(key)) continue;
    kept[key] = value;
  }
  return kept;
}

function toneForPhase(phase: ToolPhase): ActivityTone {
  switch (phase) {
    case "requested":
    case "running":
      return "running";
    case "awaiting_approval":
      return "attention";
    case "succeeded":
      return "success";
    case "failed":
    case "denied":
      return "failure";
    default:
      return "neutral";
  }
}

/** True while a tool call has not reached a terminal state. */
export function isToolPending(block: ToolBlock): boolean {
  return block.phase === "requested" || block.phase === "running" || block.phase === "awaiting_approval";
}

export interface BuildOptions {
  /**
   * Approvals the runtime has asked about and not yet resolved. These are
   * pushed as `approval.request` notifications rather than events, so they are
   * supplied separately and matched to their tool call by name.
   */
  approvals?: { approval_id: string; tool: { name: string; arguments: Record<string, unknown> } }[];
  /** Emit an approval block for each unresolved approval. Defaults to true. */
  includeApprovalBlocks?: boolean;
}

/**
 * Folds the event log into one ordered activity stream.
 *
 * `assistant.delta` is accumulated into the message it belongs to, and a
 * following `assistant.message` for the same turn closes it, so a streamed
 * reply appears once and grows in place rather than as hundreds of rows.
 */
export function buildActivityStream(events: HarnessEvent[], options: BuildOptions = {}): ActivityBlock[] {
  const { approvals = [], includeApprovalBlocks = true } = options;
  const blocks: ActivityBlock[] = [];

  // Tool calls are matched by name because the runtime does not include a tool
  // call id in the lifecycle events. Index of the most recent open call per tool
  // name keeps concurrent calls with the same name ordered correctly.
  const openTools = new Map<string, ToolBlock[]>();
  // Whether the assistant block currently at the end of the stream was built
  // from deltas, so a following `assistant.message` closes it instead of
  // appending a second copy of the same reply.
  let assistantFromStream = false;

  function openAssistant(): AssistantBlock | null {
    const last = blocks[blocks.length - 1];
    return last && last.kind === "assistant" ? last : null;
  }

  function closeAssistant() {
    const open = openAssistant();
    if (open) open.streaming = false;
    assistantFromStream = false;
  }

  function takeTool(name: string): ToolBlock | null {
    const queue = openTools.get(name);
    if (!queue || queue.length === 0) return null;
    return queue[queue.length - 1];
  }

  function finishTool(block: ToolBlock) {
    const queue = openTools.get(block.tool);
    if (queue) {
      const index = queue.indexOf(block);
      if (index >= 0) queue.splice(index, 1);
    }
  }

  for (const event of events) {
    switch (event.event_type) {
      case "user.message": {
        closeAssistant();
        const value = text(event);
        if (value) {
          blocks.push({ kind: "user", id: event.event_id, text: value, timestamp: event.timestamp });
        }
        break;
      }

      case "assistant.delta": {
        const value = text(event);
        if (!value) break;
        const open = openAssistant();
        if (open && assistantFromStream) {
          open.text += value;
          open.timestamp = event.timestamp;
        } else {
          closeAssistant();
          blocks.push({
            kind: "assistant",
            id: event.event_id,
            text: value,
            timestamp: event.timestamp,
            streaming: true,
            provider: null,
            model: null,
            inputTokens: null,
            outputTokens: null,
          });
          assistantFromStream = true;
        }
        break;
      }

      case "assistant.message": {
        const value = text(event);
        const open = openAssistant();
        if (open && assistantFromStream) {
          // Close the streamed turn with the authoritative text.
          open.text = value;
        } else {
          closeAssistant();
          blocks.push({
            kind: "assistant",
            id: event.event_id,
            text: value,
            timestamp: event.timestamp,
            streaming: false,
            provider: null,
            model: null,
            inputTokens: null,
            outputTokens: null,
          });
        }
        closeAssistant();
        break;
      }

      case "model.response": {
        const open = openAssistant();
        if (open) {
          open.provider = stringField(event, "provider") || open.provider;
          open.model = stringField(event, "model") || open.model;
          open.inputTokens = numberField(event, "input_tokens") ?? open.inputTokens;
          open.outputTokens = numberField(event, "output_tokens") ?? open.outputTokens;
        }
        break;
      }

      case "tool.requested": {
        const tool = stringField(event, "tool");
        if (!tool) break;
        closeAssistant();
        const args = recordField(event, "arguments");
        const block: ToolBlock = {
          kind: "tool",
          id: event.event_id,
          tool,
          label: describeTool(tool, args),
          arguments: args,
          phase: "requested",
          tone: "running",
          timestamp: event.timestamp,
          durationMs: null,
          output: "",
          error: "",
          path: toolPath(tool, args),
          approvalId: null,
          approvalReason: null,
          approvalRisks: [],
        };
        blocks.push(block);
        const queue = openTools.get(tool) ?? [];
        queue.push(block);
        openTools.set(tool, queue);
        break;
      }

      case "tool.started": {
        const block = takeTool(stringField(event, "tool"));
        if (!block) break;
        block.phase = "running";
        block.tone = toneForPhase("running");
        block.timestamp = event.timestamp;
        break;
      }

      case "tool.output": {
        const block = takeTool(stringField(event, "tool"));
        if (!block) break;
        block.output = (block.output + stringField(event, "output")).slice(-OUTPUT_LIMIT);
        break;
      }

      case "tool.approved": {
        const block = takeTool(stringField(event, "tool"));
        if (!block) break;
        // An approval is a gate passed, not the outcome of the call.
        block.phase = "running";
        block.approvalId = null;
        block.tone = "running";
        block.approvalReason = stringField(event, "reason") || block.approvalReason;
        break;
      }

      case "tool.completed": {
        const block = takeTool(stringField(event, "tool"));
        if (!block) break;
        finishTool(block);
        block.phase = "succeeded";
        block.tone = "success";
        block.durationMs = Math.max(0, event.timestamp - block.timestamp);
        break;
      }

      case "tool.failed": {
        const block = takeTool(stringField(event, "tool"));
        if (!block) break;
        finishTool(block);
        block.phase = "failed";
        block.tone = "failure";
        block.error = stringField(event, "error");
        block.durationMs = Math.max(0, event.timestamp - block.timestamp);
        break;
      }

      case "tool.denied": {
        const block = takeTool(stringField(event, "tool"));
        if (!block) break;
        finishTool(block);
        block.phase = "denied";
        block.tone = "failure";
        block.error = stringField(event, "reason");
        block.approvalId = null;
        break;
      }

      case "verification.started": {
        closeAssistant();
        for (const command of stringList(event, "commands")) {
          blocks.push({
            kind: "verification",
            id: `${event.event_id}:${command}`,
            command,
            category: "pending",
            passed: null,
            tone: "running",
            durationMs: null,
            exitCode: null,
            diagnostics: [],
            output: "",
          });
        }
        break;
      }

      case "verification.result": {
        const command = stringField(event, "command");
        const passed = event.payload.data.passed === true;
        const result = {
          passed,
          tone: (passed ? "success" : "failure") as ActivityTone,
          category: stringField(event, "category"),
          durationMs: numberField(event, "duration_ms"),
          exitCode: numberField(event, "exit_code"),
          diagnostics: stringList(event, "diagnostics"),
          output: stringField(event, "output"),
        };
        const existing = blocks.find(
          (block): block is VerificationBlock =>
            block.kind === "verification" && block.command === command && block.passed === null,
        );
        if (existing) Object.assign(existing, result);
        else {
          blocks.push({
            kind: "verification",
            id: event.event_id,
            command,
            ...result,
          });
        }
        break;
      }

      case "session.failed": {
        closeAssistant();
        blocks.push({
          kind: "error",
          id: event.event_id,
          title: "Run failed",
          detail: stringField(event, "error"),
          timestamp: event.timestamp,
        });
        break;
      }

      case "session.started": {
        const root = stringField(event, "workspace_root");
        if (root) {
          blocks.push({
            kind: "notice",
            id: event.event_id,
            label: "Session started",
            detail: root,
            timestamp: event.timestamp,
            tone: "neutral",
            reference: null,
          });
        }
        break;
      }

      case "background_process.started": {
        const processId = stringField(event, "process_id");
        if (!processId) break;
        blocks.push({
          kind: "notice",
          id: event.event_id,
          label: "Background process running",
          detail: `${processId} · ${stringField(event, "command")} · PID ${numberField(event, "pid") ?? "?"}`,
          timestamp: event.timestamp,
          tone: "running",
          reference: processId,
        });
        break;
      }

      case "background_process.status": {
        const processId = stringField(event, "process_id");
        const status = stringField(event, "status");
        const processNotice = [...blocks].reverse().find(
          (block): block is NoticeBlock => block.kind === "notice" && block.reference === processId,
        );
        if (!processNotice) break;
        processNotice.label = `Background process ${status}`;
        processNotice.detail = `${processNotice.detail} · ${status}${event.payload.data.exit_code === null || event.payload.data.exit_code === undefined ? "" : ` (${numberField(event, "exit_code")})`}`;
        processNotice.timestamp = event.timestamp;
        processNotice.tone = status === "failed" ? "failure" : status === "exited" ? "success" : status === "running" ? "running" : "neutral";
        break;
      }

      case "session.resumed": {
        closeAssistant();
        blocks.push({
          kind: "notice",
          id: event.event_id,
          label: "Session resumed",
          detail: stringField(event, "reason"),
          timestamp: event.timestamp,
          tone: "neutral",
          reference: null,
        });
        break;
      }

      case "checkpoint.created": {
        const reference = stringField(event, "reference");
        blocks.push({
          kind: "notice",
          id: event.event_id,
          label: "Checkpoint",
          detail: reference,
          timestamp: event.timestamp,
          tone: "neutral",
          reference: reference || null,
        });
        break;
      }

      case "checkpoint.restored": {
        const restored = stringList(event, "restored_files");
        const conflicts = stringList(event, "conflicts");
        blocks.push({
          kind: "notice",
          id: event.event_id,
          label: conflicts.length > 0 ? "Restore had conflicts" : "Checkpoint restored",
          detail:
            conflicts.length > 0
              ? conflicts.join(", ")
              : `${restored.length} file${restored.length === 1 ? "" : "s"} restored`,
          timestamp: event.timestamp,
          tone: conflicts.length > 0 ? "attention" : "neutral",
          reference: null,
        });
        break;
      }

      case "context.compacted": {
        const removed = numberField(event, "removed_items") ?? 0;
        blocks.push({
          kind: "notice",
          id: event.event_id,
          label: "Context compacted",
          detail: `${removed} item${removed === 1 ? "" : "s"} summarised`,
          timestamp: event.timestamp,
          tone: "neutral",
          reference: null,
        });
        break;
      }

      case "session.completed": {
        closeAssistant();
        blocks.push({
          kind: "notice",
          id: event.event_id,
          label: "Run complete",
          detail: stringField(event, "reason"),
          timestamp: event.timestamp,
          tone: "success",
          reference: null,
        });
        break;
      }

      case "file.changed": {
        // Surfaced in the inspector; the stream stays focused on agent activity.
        break;
      }

      case "policy.decision": {
        // Recorded for the inspector's event details, not the main stream.
        break;
      }

      default:
        break;
    }
  }

  if (includeApprovalBlocks) {
    linkApprovals(blocks, approvals);
  }

  return blocks;
}

/**
 * Attaches each pending approval to the tool call it is about.
 *
 * An approval arrives as a `approval.request` notification rather than an event,
 * so it has to be matched back to its call. Matching by name and taking the most
 * recent still-open call keeps concurrent calls of the same tool distinct. When
 * no open call matches — the event has scrolled out of the retained window — the
 * approval is shown on its own rather than silently dropped, because a person
 * still has to answer it.
 */
function linkApprovals(
  blocks: ActivityBlock[],
  approvals: ApprovalRequest[],
): void {
  for (const approval of approvals) {
    const name = approval.tool.name;
    const candidate = [...blocks]
      .reverse()
      .find(
        (block): block is ToolBlock =>
          block.kind === "tool" && block.tool === name && isToolPending(block) && !block.approvalId,
      );

    if (candidate) {
      candidate.phase = "awaiting_approval";
      candidate.tone = "attention";
      candidate.approvalId = approval.approval_id;
      candidate.approvalReason = approval.reason ?? null;
      candidate.approvalRisks = approval.risk_categories ?? [];
      // The notification's arguments are the authoritative ones.
      if (Object.keys(approval.tool.arguments).length > 0) {
        candidate.arguments = approval.tool.arguments;
        candidate.label = describeTool(name, approval.tool.arguments);
        candidate.path = toolPath(name, approval.tool.arguments);
      }
      continue;
    }

    blocks.push({
      kind: "approval",
      id: `approval:${approval.approval_id}`,
      approvalId: approval.approval_id,
      tool: name,
      arguments: approval.tool.arguments,
      timestamp: Date.now(),
      reason: approval.reason ?? "",
      risks: approval.risk_categories ?? [],
    });
  }
}

/** Counts a run's outcomes for the header, without walking the stream twice. */
export interface StreamSummary {
  tools: number;
  failures: number;
  awaitingApproval: number;
  lastActivityAt: number | null;
}

export function summariseStream(blocks: ActivityBlock[]): StreamSummary {
  let tools = 0;
  let failures = 0;
  let awaitingApproval = 0;
  let lastActivityAt: number | null = null;

  for (const block of blocks) {
    if ("timestamp" in block && typeof block.timestamp === "number") {
      lastActivityAt = lastActivityAt === null ? block.timestamp : Math.max(lastActivityAt, block.timestamp);
    }
    if (block.kind === "tool") {
      tools += 1;
      if (block.phase === "failed" || block.phase === "denied") failures += 1;
      if (block.phase === "awaiting_approval") awaitingApproval += 1;
    }
    if (block.kind === "error") failures += 1;
  }

  return { tools, failures, awaitingApproval, lastActivityAt };
}
