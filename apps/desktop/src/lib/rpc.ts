import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export const RPC_PROTOCOL_VERSION = 1;

export type RuntimeStatus =
  | "unavailable"
  | "connecting"
  | "connected"
  | "disconnected"
  | "error";

export type RuntimeConnectionState =
  | "disconnected"
  | "discovering"
  | "starting"
  | "connecting"
  | "connected"
  | "reconnecting"
  | "failed";

export interface RuntimeConnectionEvent {
  state: RuntimeConnectionState;
  message: string;
}

export interface RuntimeDiagnostics {
  state: string;
  endpoint: string | null;
  workspace_root: string | null;
  started_runtime: boolean;
  retry_count: number;
  last_error: string | null;
}

const runtimeEvents = [
  "runtime.disconnected",
  "runtime.discovering",
  "runtime.starting",
  "runtime.connecting",
  "runtime.connected",
  "runtime.reconnecting",
  "runtime.failed",
] as const;

/** Registers the runtime-state bridge before startup begins. */
export async function listenRuntimeConnectionStates(
  onState: (event: RuntimeConnectionEvent) => void,
): Promise<UnlistenFn> {
  const unlisten: UnlistenFn[] = [];
  try {
    for (const name of runtimeEvents) {
      unlisten.push(
        await listen<RuntimeConnectionEvent>(name, ({ payload }) => {
          onState(payload);
        }),
      );
    }
  } catch (error) {
    unlisten.forEach((stop) => stop());
    throw error;
  }
  return () => unlisten.forEach((stop) => stop());
}

export interface RpcError {
  code: string;
  message: string;
  data?: unknown;
}

export interface RpcResponse<T = unknown> {
  version: number;
  id: string | null;
  ok: boolean;
  result?: T;
  error?: RpcError;
}

export interface RpcNotification {
  version: number;
  method: string;
  params: Record<string, unknown>;
}

export type ServerMessage =
  | { kind: "response"; response: RpcResponse }
  | { kind: "notification"; notification: RpcNotification };

export interface WorkspaceSummary {
  current_directory: string;
  repository_root: string | null;
  languages: string[];
  manifests: string[];
  instructions: string[];
  configuration: {
    package_manager: string | null;
    commands: Record<string, unknown>;
    source: string | null;
  };
}

export interface SessionSummary {
  id: string;
  workspace_root: string;
  /** Deterministic title derived from the first user task. */
  title?: string | null;
  status: "Active" | "Completed" | "Failed";
  created_at: number;
  last_updated_at: number;
  event_count: number;
  context_compactions: number;
}

export interface HarnessEvent {
  schema_version: number;
  event_id: string;
  session_id: string;
  timestamp: number;
  event_type: string;
  parent_id: string | null;
  correlation_id: string | null;
  payload: {
    type: string;
    data: Record<string, unknown>;
  };
}

export interface ApprovalRequest {
  approval_id: string;
  tool: {
    name: string;
    arguments: Record<string, unknown>;
  };
}

export interface AgentTask {
  workspace_root: string;
  user_task: string;
  task_mode: TaskMode;
  system_instructions: string;
  workspace: Record<string, unknown>;
  instructions: unknown[];
  recent_conversation: unknown[];
  selected_files: unknown[];
  initial_tool_results: unknown[];
  git_status: null;
  verification_plan: null;
  resume_session: string | null;
}

export type TaskMode = "explore" | "plan" | "code";

export type PlanItemStatus = "pending" | "in_progress" | "completed" | "blocked";

export interface GoalSnapshot {
  objective: string;
  constraints: string[];
  acceptance_criteria: string[];
  non_goals: string[];
  current_milestone: string | null;
  completion_condition: string;
}

export interface ExecutionPlanSnapshot {
  revision: number;
  status: PlanItemStatus;
  decision_notes: string[];
  milestones: {
    title: string;
    status: PlanItemStatus;
    tasks: { description: string; status: PlanItemStatus }[];
    affected_architecture: string[];
    validation_commands: string[];
    completion_criteria: string[];
  }[];
}

export interface TaskRunSnapshot {
  original_goal: string;
  goal: GoalSnapshot;
  execution_plan: ExecutionPlanSnapshot | null;
  current_phase: string;
  completion_status: string;
}

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

export interface GitDiff {
  unstaged: string;
  staged: string;
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

export class RpcTransportError extends Error {
  readonly code: string;

  constructor(message: string, code = "transport_error") {
    super(message);
    this.name = "RpcTransportError";
    this.code = code;
  }
}

export async function connectRuntime(
  address: string,
  workspacePath: string,
  reconnect = false,
): Promise<string> {
  try {
    return await invoke<string>("rpc_connect", { address, workspacePath, reconnect });
  } catch (error) {
    const detail = typeof error === "string" ? error : error instanceof Error ? error.message : "";
    if (/protocol mismatch|incompatible rpc protocol/i.test(detail)) {
      throw new RpcTransportError(
        "Harness runtime protocol is incompatible. Close the older runtime process, update the CLI and desktop app to matching releases, then retry.",
        "incompatible_protocol",
      );
    }
    throw new RpcTransportError(
      "Harness could not connect or start. Retry, restart the runtime, or open logs for details.",
      "runtime_unavailable",
    );
  }
}

export async function openRuntimeLogs(): Promise<void> {
  try {
    await invoke("rpc_open_runtime_logs");
  } catch {
    throw new RpcTransportError("Harness logs could not be opened", "runtime_unavailable");
  }
}

export async function restartRuntime(address: string, workspacePath: string): Promise<void> {
  try {
    await invoke("rpc_restart_runtime", { address, workspacePath });
  } catch {
    throw new RpcTransportError("Harness could not be restarted. Open logs for details.", "runtime_unavailable");
  }
}

export async function readRuntimeDiagnostics(): Promise<RuntimeDiagnostics> {
  try {
    return await invoke<RuntimeDiagnostics>("rpc_connection_status");
  } catch {
    throw new RpcTransportError("Runtime diagnostics are unavailable", "runtime_unavailable");
  }
}

export async function requestRuntime<T>(
  clientId: string,
  method: string,
  params: Record<string, unknown> = {},
  idempotencyKey?: string,
): Promise<RpcResponse<T>> {
  try {
    return await invoke<RpcResponse<T>>("rpc_request", {
      clientId,
      method,
      params,
      ...(idempotencyKey ? { idempotencyKey } : {}),
    });
  } catch {
    throw new RpcTransportError("Harness connection was interrupted", "disconnected");
  }
}

export async function receiveRuntimeMessage(clientId: string): Promise<ServerMessage> {
  try {
    const value = await invoke<unknown>("rpc_receive", { clientId });
    if (value === null) {
      throw new RpcTransportError("runtime connection closed", "disconnected");
    }
    return parseServerMessage(value);
  } catch {
    throw new RpcTransportError("Harness connection was interrupted", "disconnected");
  }
}

export async function disconnectRuntime(clientId: string): Promise<void> {
  try {
    await invoke("rpc_disconnect", { clientId });
  } catch {
    return;
  }
}

export function parseServerMessage(value: unknown): ServerMessage {
  if (!value || typeof value !== "object") {
    throw new RpcTransportError("runtime returned a malformed message", "malformed_event");
  }
  const record = value as Record<string, unknown>;
  if (record.version !== RPC_PROTOCOL_VERSION) {
    throw new RpcTransportError(
      `unsupported runtime protocol version ${String(record.version)}`,
      "malformed_event",
    );
  }
  if (typeof record.method === "string" && record.params && typeof record.params === "object") {
    return {
      kind: "notification",
      notification: {
        version: record.version,
        method: record.method,
        params: record.params as Record<string, unknown>,
      },
    };
  }
  if (typeof record.ok === "boolean" && (typeof record.id === "string" || record.id === null)) {
    return {
      kind: "response",
      response: {
        version: record.version,
        id: record.id,
        ok: record.ok,
        result: record.result,
        error: record.error as RpcError | undefined,
      },
    };
  }
  throw new RpcTransportError("runtime returned an unknown message shape", "malformed_event");
}

export function expectResult<T>(response: RpcResponse<T>): T {
  if (!response.ok) {
    throw new RpcTransportError(
      response.error?.message ?? "runtime request failed",
      response.error?.code ?? "runtime_error",
    );
  }
  if (response.result === undefined) {
    throw new RpcTransportError("runtime response did not include a result", "malformed_event");
  }
  return response.result;
}
