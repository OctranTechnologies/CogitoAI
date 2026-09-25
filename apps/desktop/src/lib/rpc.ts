import { invoke } from "@tauri-apps/api/core";

export const RPC_PROTOCOL_VERSION = 1;

export type RuntimeStatus =
  | "unavailable"
  | "connecting"
  | "connected"
  | "disconnected"
  | "error";

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

export class RpcTransportError extends Error {
  readonly code: string;

  constructor(message: string, code = "transport_error") {
    super(message);
    this.name = "RpcTransportError";
    this.code = code;
  }
}

export async function connectRuntime(address: string): Promise<string> {
  try {
    return await invoke<string>("rpc_connect", { address });
  } catch (error) {
    throw new RpcTransportError(String(error), "runtime_unavailable");
  }
}

export async function requestRuntime<T>(
  clientId: string,
  method: string,
  params: Record<string, unknown> = {},
): Promise<RpcResponse<T>> {
  try {
    return await invoke<RpcResponse<T>>("rpc_request", { clientId, method, params });
  } catch (error) {
    throw new RpcTransportError(String(error));
  }
}

export async function receiveRuntimeMessage(clientId: string): Promise<ServerMessage> {
  try {
    const value = await invoke<unknown>("rpc_receive", { clientId });
    if (value === null) {
      throw new RpcTransportError("runtime connection closed", "disconnected");
    }
    return parseServerMessage(value);
  } catch (error) {
    throw new RpcTransportError(String(error));
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
