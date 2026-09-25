import { create } from "zustand";
import { persist } from "zustand/middleware";
import {
  connectRuntime,
  disconnectRuntime,
  expectResult,
  receiveRuntimeMessage,
  requestRuntime,
  RpcTransportError,
  type AgentTask,
  type ApprovalRequest,
  type HarnessEvent,
  type RuntimeStatus,
  type ServerMessage,
  type SessionSummary,
  type WorkspaceSummary,
} from "./lib/rpc";

export interface ChatMessage {
  id: string;
  role: "user" | "assistant" | "system";
  text: string;
  createdAt: number;
  streaming?: boolean;
}

export interface DesktopStore {
  status: RuntimeStatus;
  address: string;
  clientId: string | null;
  workspacePath: string;
  workspace: WorkspaceSummary | null;
  sessions: SessionSummary[];
  activeSessionId: string | null;
  activeRunId: string | null;
  messages: ChatMessage[];
  events: HarnessEvent[];
  approvals: ApprovalRequest[];
  composer: string;
  isLoadingWorkspace: boolean;
  lastError: string | null;
  connect: (address: string, workspacePath: string) => Promise<void>;
  disconnect: () => Promise<void>;
  setWorkspacePath: (path: string) => void;
  setComposer: (value: string) => void;
  selectSession: (sessionId: string) => void;
  createSession: () => Promise<void>;
  sendMessage: (text: string) => Promise<void>;
  approve: (approvalId: string) => Promise<void>;
  deny: (approvalId: string) => Promise<void>;
  cancel: () => Promise<void>;
  handleServerMessage: (message: ServerMessage) => void;
  setRuntimeError: (message: string) => void;
  markDisconnected: () => void;
  clearError: () => void;
}

const systemInstructions =
  "You are the CogitoAI coding agent. Follow project instructions and use tools safely.";

function errorMessage(error: unknown): string {
  if (error instanceof RpcTransportError) return error.message;
  if (error instanceof Error) return error.message;
  return String(error);
}

function isHarnessEvent(value: unknown): value is HarnessEvent {
  if (!value || typeof value !== "object") return false;
  const event = value as Partial<HarnessEvent>;
  return (
    event.schema_version === 1 &&
    typeof event.event_id === "string" &&
    typeof event.session_id === "string" &&
    typeof event.event_type === "string" &&
    typeof event.payload === "object" &&
    event.payload !== null
  );
}

function eventText(event: HarnessEvent): string | null {
  const data = event.payload.data;
  if (typeof data.text === "string") return data.text;
  return null;
}

function makeTask(workspacePath: string, text: string, sessionId: string | null): AgentTask {
  return {
    workspace_root: workspacePath,
    user_task: text,
    system_instructions: systemInstructions,
    workspace: { root: workspacePath, branch: null, monorepo: false, languages: [], manifests: [], details: {} },
    instructions: [],
    recent_conversation: [],
    selected_files: [],
    initial_tool_results: [],
    git_status: null,
    verification_plan: null,
    resume_session: sessionId,
  };
}

export const useDesktopStore = create<DesktopStore>()(
  persist(
    (set, get) => ({
  status: "unavailable",
  address: "127.0.0.1:4545",
  clientId: null,
  workspacePath: "",
  workspace: null,
  sessions: [],
  activeSessionId: null,
  activeRunId: null,
  messages: [],
  events: [],
  approvals: [],
  composer: "",
  isLoadingWorkspace: false,
  lastError: null,

  connect: async (address, workspacePath) => {
    const current = get();
    if (current.clientId) await get().disconnect();
    set({ status: "connecting", address, workspacePath, lastError: null, isLoadingWorkspace: true });
    try {
      const clientId = await connectRuntime(address);
      set({ clientId });
      const initialize = await requestRuntime(clientId, "rpc.initialize");
      expectResult(initialize);
      const workspace = await requestRuntime<WorkspaceSummary>(clientId, "workspace.open", {
        path: workspacePath,
      });
      set({ workspace: expectResult(workspace) });
      const sessions = await requestRuntime<SessionSummary[]>(clientId, "session.list", { limit: 30 });
      set({ sessions: expectResult(sessions), status: "connected", isLoadingWorkspace: false });
    } catch (error) {
      const clientId = get().clientId;
      if (clientId) await disconnectRuntime(clientId).catch(() => undefined);
      set({
        status: "error",
        clientId: null,
        isLoadingWorkspace: false,
        lastError: errorMessage(error),
      });
    }
  },

  disconnect: async () => {
    const clientId = get().clientId;
    if (clientId) await disconnectRuntime(clientId);
    set({ status: "disconnected", clientId: null, activeRunId: null });
  },

  setWorkspacePath: (workspacePath) => set({ workspacePath }),
  setComposer: (composer) => set({ composer }),
  selectSession: (activeSessionId) => set({ activeSessionId, messages: [], events: [], approvals: [] }),
  setRuntimeError: (message) => set({ status: "error", lastError: message, clientId: null, activeRunId: null }),
  markDisconnected: () => set({ status: "disconnected", clientId: null, activeRunId: null }),
  clearError: () => set({ lastError: null }),

  createSession: async () => {
    const { clientId, workspacePath } = get();
    if (!clientId || !workspacePath) return;
    try {
      const response = await requestRuntime<{ session: { id: string } }>(clientId, "session.create", {
        path: workspacePath,
      });
      const result = expectResult(response);
      set((state) => ({
        activeSessionId: result.session.id,
        messages: [],
        events: [],
        approvals: [],
        sessions: [
          {
            id: result.session.id,
            workspace_root: workspacePath,
            status: "Active",
            created_at: Date.now(),
            last_updated_at: Date.now(),
            event_count: 1,
            context_compactions: 0,
          },
          ...state.sessions,
        ],
      }));
    } catch (error) {
      set({ lastError: errorMessage(error) });
    }
  },

  sendMessage: async (text) => {
    const { clientId, workspacePath, activeSessionId, activeRunId } = get();
    if (!clientId || !workspacePath || activeRunId) return;
    if (!text.trim()) return;
    if (!activeSessionId) {
      await get().createSession();
    }
    const sessionId = get().activeSessionId;
    if (!sessionId) return;
    const userMessage: ChatMessage = {
      id: `user-${Date.now()}`,
      role: "user",
      text: text.trim(),
      createdAt: Date.now(),
    };
    set((state) => ({ messages: [...state.messages, userMessage], composer: "" }));
    try {
      const response = await requestRuntime<{ run_id: string }>(clientId, "agent.send", {
        task: makeTask(workspacePath, text, sessionId),
      });
      set({ activeRunId: expectResult(response).run_id, lastError: null });
    } catch (error) {
      set({ lastError: errorMessage(error) });
    }
  },

  approve: async (approvalId) => {
    const { clientId } = get();
    if (!clientId) return;
    try {
      await requestRuntime(clientId, "agent.approve", { approval_id: approvalId });
      set((state) => ({ approvals: state.approvals.filter((item) => item.approval_id !== approvalId) }));
    } catch (error) {
      set({ lastError: errorMessage(error) });
    }
  },

  deny: async (approvalId) => {
    const { clientId } = get();
    if (!clientId) return;
    try {
      await requestRuntime(clientId, "agent.deny", { approval_id: approvalId });
      set((state) => ({ approvals: state.approvals.filter((item) => item.approval_id !== approvalId) }));
    } catch (error) {
      set({ lastError: errorMessage(error) });
    }
  },

  cancel: async () => {
    const { clientId, activeRunId } = get();
    if (!clientId || !activeRunId) return;
    try {
      await requestRuntime(clientId, "agent.cancel", { run_id: activeRunId });
    } catch (error) {
      set({ lastError: errorMessage(error) });
    }
  },

  handleServerMessage: (message) => {
    if (message.kind === "response") {
      if (!message.response.ok) {
        set({ lastError: message.response.error?.message ?? "runtime request failed" });
      }
      return;
    }
    const { method, params } = message.notification;
    if (method === "agent.event") {
      const event = params.event;
      if (!isHarnessEvent(event)) {
        set({ lastError: "runtime returned a malformed harness event" });
        return;
      }
      set((state) => ({ events: [...state.events, event].slice(-250) }));
      if (event.event_type === "assistant.delta") {
        const text = eventText(event);
        if (text) {
          set((state) => {
            const last = state.messages[state.messages.length - 1];
            if (last?.role === "assistant" && last.streaming) {
              return {
                messages: [
                  ...state.messages.slice(0, -1),
                  { ...last, text: last.text + text, streaming: true },
                ],
              };
            }
            return {
              messages: [
                ...state.messages,
                { id: event.event_id, role: "assistant", text, createdAt: Date.now(), streaming: true },
              ],
            };
          });
        }
      }
      if (event.event_type === "assistant.message") {
        const text = eventText(event) ?? "";
        set((state) => {
          const last = state.messages[state.messages.length - 1];
          if (last?.role === "assistant" && last.streaming) {
            return {
              messages: [...state.messages.slice(0, -1), { ...last, text, streaming: false }],
            };
          }
          return {
            messages: [...state.messages, { id: event.event_id, role: "assistant", text, createdAt: Date.now() }],
          };
        });
      }
      if (event.event_type === "session.completed") {
        set({ activeRunId: null });
      }
      return;
    }
    if (method === "approval.request") {
      const approvalId = params.approval_id;
      const tool = params.tool;
      if (typeof approvalId === "string" && tool && typeof tool === "object") {
        set((state) => ({
          approvals: [
            ...state.approvals.filter((item) => item.approval_id !== approvalId),
            { approval_id: approvalId, tool: tool as ApprovalRequest["tool"] },
          ],
        }));
      } else {
        set({ lastError: "runtime returned a malformed approval request" });
      }
      return;
    }
    if (method === "agent.completed") {
      set({ activeRunId: null, lastError: null });
      return;
    }
    if (method === "agent.failed") {
      const error = params.error;
      set({
        activeRunId: null,
        lastError: error && typeof error === "object" && "message" in error ? String(error.message) : "agent run failed",
      });
    }
  },
    }),
    {
      name: "cogitoai-desktop-ui",
      partialize: (state) => ({
        address: state.address,
        workspacePath: state.workspacePath,
        activeSessionId: state.activeSessionId,
      }),
    },
  ),
);

export function startEventPump(
  clientId: string,
  onMessage: (message: ServerMessage) => void,
  onError: (error: unknown) => void,
): () => void {
  let stopped = false;
  const pump = async () => {
    while (!stopped) {
      try {
        onMessage(await receiveRuntimeMessage(clientId));
      } catch (error) {
        if (!stopped) onError(error);
        return;
      }
    }
  };
  void pump();
  return () => {
    stopped = true;
  };
}
