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
  type CheckpointInfo,
  type GitDiff,
  type GitStatusSummary,
  type HarnessEvent,
  type RestoreReport,
  type RuntimeStatus,
  type ServerMessage,
  type SessionSummary,
  type WorkspaceSummary,
} from "./lib/rpc";
import {
  deriveTimeline,
  deriveToolActivities,
  deriveVerificationActivities,
  hydrateConversation,
  type ChatMessage,
  type RunPhase,
  type TimelineEntry,
  type ToolActivity,
  type VerificationActivity,
} from "./lib/events";
import {
  deriveCheckpoints,
  shouldRefreshChanges,
  summarizeChanges,
  type ChangeSummary,
  type CheckpointEntry,
  type FileChange,
  type FileView,
} from "./lib/changes";
import {
  DEFAULT_TERMINAL_COLS,
  DEFAULT_TERMINAL_ROWS,
  HUMAN_ORIGIN,
  isPtyInfo,
  type PtyInfo,
  type TerminalExit,
} from "./lib/terminal";

export type { ChatMessage, RunPhase, TimelineEntry, ToolActivity, VerificationActivity } from "./lib/events";
export type { ChangeSummary, CheckpointEntry, FileChange, FileView } from "./lib/changes";
export type { PtyInfo, TerminalExit } from "./lib/terminal";

/**
 * Terminal sessions are human-controlled, not agent-controlled.
 *
 * The agent runs shell commands only through the runtime's tool registry, which
 * evaluates every call against the policy engine (deny / ask / allow) and runs
 * it captured and non-interactively. That path is deliberately untouched by the
 * terminal panel.
 *
 * A terminal is an interactive shell a person types into directly, so agent
 * policy is intentionally *not* applied: prompting someone to approve their own
 * keystrokes would be noise, not safety. The distinction is enforced by the
 * runtime, which requires `origin: "human"` and exposes no tool that can open a
 * terminal. Nothing in the agent's reach can reach this path.
 */

interface SessionInspectReport {
  session: {
    id: string;
    workspace_root: string;
    events: HarnessEvent[];
  };
  warnings: unknown[];
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
  runPhase: RunPhase;
  messages: ChatMessage[];
  events: HarnessEvent[];
  toolActivity: ToolActivity[];
  verificationActivity: VerificationActivity[];
  timeline: TimelineEntry[];
  approvals: ApprovalRequest[];
  composer: string;
  isLoadingWorkspace: boolean;
  isLoadingSession: boolean;
  lastError: string | null;
  gitStatus: GitStatusSummary | null;
  changes: ChangeSummary;
  selectedPath: string | null;
  fileChange: FileChange | null;
  fileView: FileView | null;
  aggregateDiff: GitDiff | null;
  isLoadingChanges: boolean;
  isChangesTruncated: boolean;
  isLoadingFile: boolean;
  checkpoints: CheckpointEntry[];
  restoringCheckpointId: string | null;
  lastRestore: RestoreReport | null;
  terminal: PtyInfo | null;
  isStartingTerminal: boolean;
  terminalExit: TerminalExit | null;
  connect: (address: string, workspacePath: string) => Promise<void>;
  disconnect: () => Promise<void>;
  setWorkspacePath: (path: string) => void;
  setComposer: (value: string) => void;
  selectSession: (sessionId: string) => void;
  createSession: () => Promise<void>;
  resumeSession: (sessionId?: string) => Promise<void>;
  sendMessage: (text: string) => Promise<void>;
  approve: (approvalId: string) => Promise<void>;
  deny: (approvalId: string) => Promise<void>;
  cancel: () => Promise<void>;
  handleServerMessage: (message: ServerMessage) => void;
  setRuntimeError: (message: string) => void;
  markDisconnected: () => void;
  clearError: () => void;
  refreshChanges: () => Promise<void>;
  selectFile: (path: string) => Promise<void>;
  clearSelectedFile: () => void;
  restoreCheckpoint: (checkpointId: string) => Promise<void>;
  startTerminal: (cols?: number, rows?: number) => Promise<void>;
  writeTerminal: (data: string) => Promise<void>;
  resizeTerminal: (cols: number, rows: number) => Promise<void>;
  closeTerminal: () => Promise<void>;
}

const systemInstructions =
  "You are the CogitoAI coding agent. Follow project instructions and use tools safely.";

/**
 * Upper bound on per-file diffs requested at once.
 *
 * Each file costs the runtime a couple of Git invocations, so an unbounded
 * request list would make a very dirty repository slow to open. The full file
 * list is still reported; only the per-file line counts are capped.
 */
const MAX_FILE_DIFF_REQUESTS = 120;

/**
 * Terminal output is a high-rate stream, so it is delivered by subscription
 * rather than stored in the store: buffering it would grow without bound and
 * re-render the whole conversation on every chunk.
 */
const terminalOutputListeners = new Set<(data: string) => void>();
const terminalExitListeners = new Set<(exit: TerminalExit) => void>();

/** Subscribes to streamed terminal output; returns an unsubscribe function. */
export function subscribeTerminalOutput(listener: (data: string) => void): () => void {
  terminalOutputListeners.add(listener);
  return () => {
    terminalOutputListeners.delete(listener);
  };
}

/** Subscribes to terminal exit notices; returns an unsubscribe function. */
export function subscribeTerminalExit(listener: (exit: TerminalExit) => void): () => void {
  terminalExitListeners.add(listener);
  return () => {
    terminalExitListeners.delete(listener);
  };
}

function errorMessage(error: unknown): string {
  if (error instanceof RpcTransportError) return error.message;
  if (error instanceof Error) return error.message;
  return String(error);
}

export function isHarnessEvent(value: unknown): value is HarnessEvent {
  if (!value || typeof value !== "object") return false;
  const event = value as Partial<HarnessEvent>;
  return (
    event.schema_version === 1 &&
    typeof event.event_id === "string" &&
    typeof event.session_id === "string" &&
    typeof event.event_type === "string" &&
    typeof event.timestamp === "number" &&
    typeof event.payload === "object" &&
    event.payload !== null
  );
}

function eventText(event: HarnessEvent): string {
  return typeof event.payload.data.text === "string" ? event.payload.data.text : "";
}

function makeTask(workspacePath: string, text: string, sessionId: string): AgentTask {
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

function derivedFromEvents(events: HarnessEvent[]) {
  return {
    events,
    toolActivity: deriveToolActivities(events),
    verificationActivity: deriveVerificationActivities(events),
    timeline: deriveTimeline(events),
  };
}

function emptyChanges(): ChangeSummary {
  return summarizeChanges([]);
}

function emptyRunState() {
  return {
    messages: [] as ChatMessage[],
    events: [] as HarnessEvent[],
    toolActivity: [] as ToolActivity[],
    verificationActivity: [] as VerificationActivity[],
    timeline: [] as TimelineEntry[],
    approvals: [] as ApprovalRequest[],
    activeRunId: null,
    runPhase: "idle" as RunPhase,
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
      runPhase: "idle",
      messages: [],
      events: [],
      toolActivity: [],
      verificationActivity: [],
      timeline: [],
      approvals: [],
      composer: "",
      isLoadingWorkspace: false,
      isLoadingSession: false,
      lastError: null,
      gitStatus: null,
      changes: emptyChanges(),
      selectedPath: null,
      fileChange: null,
      fileView: null,
      aggregateDiff: null,
      isLoadingChanges: false,
      isChangesTruncated: false,
      isLoadingFile: false,
      checkpoints: [],
      restoringCheckpointId: null,
      lastRestore: null,
      terminal: null,
      isStartingTerminal: false,
      terminalExit: null,

      connect: async (address, workspacePath) => {
        if (get().clientId) await get().disconnect();
        set({ status: "connecting", address, workspacePath, lastError: null, isLoadingWorkspace: true });
        try {
          const clientId = await connectRuntime(address);
          set({ clientId });
          expectResult(await requestRuntime(clientId, "rpc.initialize"));
          const workspace = await requestRuntime<WorkspaceSummary>(clientId, "workspace.open", { path: workspacePath });
          const sessions = await requestRuntime<SessionSummary[]>(clientId, "session.list", { limit: 30 });
          const workspaceResult = expectResult(workspace);
          const sessionResult = expectResult(sessions);
          set({ workspace: workspaceResult, sessions: sessionResult, status: "connected", isLoadingWorkspace: false });
          const persistedSession = get().activeSessionId;
          if (persistedSession && sessionResult.some((session) => session.id === persistedSession)) {
            await get().resumeSession(persistedSession);
          }
          // Load existing workspace changes and checkpoints on connect so the
          // changes panel is populated before any run happens.
          await get().refreshChanges();
        } catch (error) {
          const clientId = get().clientId;
          if (clientId) await disconnectRuntime(clientId).catch(() => undefined);
          set({ status: "error", clientId: null, isLoadingWorkspace: false, lastError: errorMessage(error) });
        }
      },

      disconnect: async () => {
        const clientId = get().clientId;
        if (clientId) await disconnectRuntime(clientId);
        set({ status: "disconnected", clientId: null, activeRunId: null, runPhase: "idle" });
      },

      setWorkspacePath: (workspacePath) => set({ workspacePath }),
      setComposer: (composer) => set({ composer }),
      selectSession: (activeSessionId) => set({ activeSessionId, ...emptyRunState() }),
      setRuntimeError: (message) =>
        set({ status: "error", lastError: message, clientId: null, activeRunId: null, runPhase: "failed" }),
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
            ...emptyRunState(),
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

      resumeSession: async (sessionId) => {
        const { clientId, activeSessionId } = get();
        const targetSession = sessionId ?? activeSessionId;
        if (!clientId || !targetSession) return;
        set({ isLoadingSession: true, lastError: null });
        try {
          expectResult(await requestRuntime(clientId, "session.resume", { session_id: targetSession }));
          const report = expectResult(
            await requestRuntime<SessionInspectReport>(clientId, "session.inspect", { session_id: targetSession }),
          );
          if (!report.session.events.every(isHarnessEvent)) {
            throw new RpcTransportError("runtime returned a malformed persisted event", "malformed_event");
          }
          const events = report.session.events;
          set({
            activeSessionId: targetSession,
            ...derivedFromEvents(events),
            messages: hydrateConversation(events),
            approvals: [],
            activeRunId: null,
            runPhase: "idle",
            isLoadingSession: false,
            workspacePath: report.session.workspace_root,
          });
        } catch (error) {
          set({ isLoadingSession: false, lastError: errorMessage(error) });
        }
      },

      sendMessage: async (text) => {
        const { clientId, workspacePath, activeSessionId, activeRunId } = get();
        if (!clientId || !workspacePath || activeRunId || !text.trim()) return;
        if (!activeSessionId) await get().createSession();
        const sessionId = get().activeSessionId;
        if (!sessionId) return;
        const userMessage: ChatMessage = {
          id: `user-${Date.now()}`,
          role: "user",
          text: text.trim(),
          createdAt: Date.now(),
        };
        set((state) => ({ messages: [...state.messages, userMessage], composer: "", runPhase: "pending" }));
        try {
          const response = await requestRuntime<{ run_id: string }>(clientId, "agent.send", {
            task: makeTask(workspacePath, text, sessionId),
          });
          set({ activeRunId: expectResult(response).run_id, lastError: null });
        } catch (error) {
          set({ runPhase: "failed", lastError: errorMessage(error) });
        }
      },

      approve: async (approvalId) => {
        const { clientId } = get();
        if (!clientId) return;
        try {
          expectResult(await requestRuntime(clientId, "agent.approve", { approval_id: approvalId }));
          set((state) => ({ approvals: state.approvals.filter((item) => item.approval_id !== approvalId) }));
        } catch (error) {
          set({ lastError: errorMessage(error) });
        }
      },

      deny: async (approvalId) => {
        const { clientId } = get();
        if (!clientId) return;
        try {
          expectResult(await requestRuntime(clientId, "agent.deny", { approval_id: approvalId }));
          set((state) => ({ approvals: state.approvals.filter((item) => item.approval_id !== approvalId) }));
        } catch (error) {
          set({ lastError: errorMessage(error) });
        }
      },

      cancel: async () => {
        const { clientId, activeRunId } = get();
        if (!clientId || !activeRunId) return;
        set({ runPhase: "cancelling" });
        try {
          expectResult(await requestRuntime(clientId, "agent.cancel", { run_id: activeRunId }));
        } catch (error) {
          set({ lastError: errorMessage(error), runPhase: "failed" });
        }
      },

      refreshChanges: async () => {
        const { clientId, status } = get();
        if (!clientId || status !== "connected") return;
        set({ isLoadingChanges: true });
        try {
          // Git status, the aggregate diff, and the checkpoint list are all
          // runtime-owned; the desktop only renders what the runtime reports.
          const [statusResponse, diffResponse, checkpointResponse] = await Promise.all([
            requestRuntime<GitStatusSummary>(clientId, "git.status", {}),
            requestRuntime<GitDiff>(clientId, "git.diff", {}),
            requestRuntime<CheckpointInfo[]>(clientId, "checkpoint.list", {}),
          ]);
          const gitStatus = expectResult(statusResponse);
          const aggregateDiff = expectResult(diffResponse);
          const checkpoints = expectResult(checkpointResponse);

          // Ask the runtime for a per-file before/after for every changed file.
          // These are all read-only inspections; the runtime performs no writes.
          const allPaths = gitStatus.changed_files;
          const requested = allPaths.slice(0, MAX_FILE_DIFF_REQUESTS);
          const fileResponses = await Promise.all(
            requested.map((path) => requestRuntime<FileChange>(clientId, "git.file_diff", { path })),
          );
          const fileChanges: FileChange[] = [];
          for (const response of fileResponses) {
            if (response.ok && response.result) fileChanges.push(response.result);
          }

          set({
            gitStatus,
            aggregateDiff,
            changes: summarizeChanges(fileChanges),
            isChangesTruncated: fileChanges.length < allPaths.length,
            checkpoints: deriveCheckpoints(checkpoints, get().events),
            isLoadingChanges: false,
          });
        } catch (error) {
          set({ isLoadingChanges: false, lastError: errorMessage(error) });
        }
      },

      selectFile: async (path) => {
        const { clientId, status } = get();
        if (!clientId || status !== "connected") return;
        set({ isLoadingFile: true, selectedPath: path, fileChange: null, fileView: null });
        try {
          // Prefer the diff view; fall back to a plain source view when the
          // runtime cannot produce a before/after for this file.
          let change: FileChange | null = null;
          let view: FileView | null = null;
          try {
            change = expectResult(await requestRuntime<FileChange>(clientId, "git.file_diff", { path }));
          } catch {
            view = expectResult(await requestRuntime<FileView>(clientId, "file.read", { path }));
          }
          set({ fileChange: change, fileView: view, isLoadingFile: false });
        } catch (error) {
          set({ isLoadingFile: false, lastError: errorMessage(error) });
        }
      },

      clearSelectedFile: () => set({ selectedPath: null, fileChange: null, fileView: null }),

      restoreCheckpoint: async (checkpointId) => {
        const { clientId, status } = get();
        if (!clientId || status !== "connected") return;
        set({ restoringCheckpointId: checkpointId, lastError: null });
        try {
          // Restoration is delegated entirely to the runtime's existing safety
          // logic; the frontend never touches the filesystem to undo changes.
          const report = expectResult(
            await requestRuntime<RestoreReport>(clientId, "checkpoint.undo", { checkpoint_id: checkpointId }),
          );
          set({ restoringCheckpointId: null, lastRestore: report });
          await get().refreshChanges();
        } catch (error) {
          set({ restoringCheckpointId: null, lastError: errorMessage(error) });
        }
      },

      startTerminal: async (cols = DEFAULT_TERMINAL_COLS, rows = DEFAULT_TERMINAL_ROWS) => {
        const { clientId, status, terminal } = get();
        if (!clientId || status !== "connected" || terminal) return;
        set({ isStartingTerminal: true, lastError: null, terminalExit: null });
        try {
          // `origin: "human"` is what makes this a person-operated terminal
          // rather than an agent command. The runtime rejects anything else.
          const info = expectResult(
            await requestRuntime<PtyInfo>(clientId, "terminal.open", {
              origin: HUMAN_ORIGIN,
              cols,
              rows,
            }),
          );
          if (!isPtyInfo(info)) {
            throw new RpcTransportError("runtime returned a malformed terminal session", "malformed_event");
          }
          set({ terminal: info, isStartingTerminal: false });
        } catch (error) {
          set({ isStartingTerminal: false, lastError: errorMessage(error) });
        }
      },

      writeTerminal: async (data) => {
        const { clientId, status, terminal } = get();
        if (!clientId || status !== "connected" || !terminal || !data) return;
        try {
          await requestRuntime(clientId, "terminal.write", { terminal_id: terminal.id, data });
        } catch (error) {
          set({ lastError: errorMessage(error) });
        }
      },

      resizeTerminal: async (cols, rows) => {
        const { clientId, status, terminal } = get();
        if (!clientId || status !== "connected" || !terminal) return;
        if (cols < 1 || rows < 1) return;
        try {
          await requestRuntime(clientId, "terminal.resize", {
            terminal_id: terminal.id,
            cols,
            rows,
          });
        } catch (error) {
          set({ lastError: errorMessage(error) });
        }
      },

      closeTerminal: async () => {
        const { clientId, terminal } = get();
        if (!clientId || !terminal) return;
        set({ terminal: null });
        try {
          await requestRuntime(clientId, "terminal.close", { terminal_id: terminal.id });
        } catch (error) {
          set({ lastError: errorMessage(error) });
        }
      },

      handleServerMessage: (message) => {
        if (message.kind === "response") {
          if (!message.response.ok) {
            set({ lastError: message.response.error?.message ?? "runtime request failed", runPhase: "failed" });
          }
          return;
        }
        const { method, params } = message.notification;
        if (method === "agent.event") {
          const event = params.event;
          if (!isHarnessEvent(event)) {
            set({ lastError: "runtime returned a malformed harness event", runPhase: "failed" });
            return;
          }
          set((state) => {
            const events = [...state.events, event].slice(-250);
            let messages = state.messages;
            if (event.event_type === "user.message" && !messages.some((item) => item.role === "user" && item.text === eventText(event))) {
              messages = [...messages, { id: event.event_id, role: "user", text: eventText(event), createdAt: event.timestamp }];
            }
            if (event.event_type === "assistant.delta") {
              const value = eventText(event);
              const last = messages[messages.length - 1];
              messages =
                last?.role === "assistant" && last.streaming
                  ? [...messages.slice(0, -1), { ...last, text: last.text + value, streaming: true }]
                  : [...messages, { id: event.event_id, role: "assistant", text: value, createdAt: event.timestamp, streaming: true }];
            }
            if (event.event_type === "assistant.message") {
              const value = eventText(event);
              const last = messages[messages.length - 1];
              messages =
                last?.role === "assistant" && last.streaming
                  ? [...messages.slice(0, -1), { ...last, text: value, streaming: false }]
                  : [...messages, { id: event.event_id, role: "assistant", text: value, createdAt: event.timestamp }];
            }
            const terminal = event.event_type === "session.completed";
            return {
              ...derivedFromEvents(events),
              messages,
              activeRunId: terminal ? null : state.activeRunId,
              runPhase: terminal ? "completed" : state.runPhase === "idle" || state.runPhase === "pending" ? "running" : state.runPhase,
            };
          });
          // Runtime events are the trigger for re-reading git, diff, and
          // checkpoint state; the frontend never polls the filesystem itself.
          if (shouldRefreshChanges(event)) {
            void get().refreshChanges();
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
              toolActivity: state.toolActivity.map((activity, index, all) =>
                index === all.length - 1 && activity.name === (tool as ApprovalRequest["tool"]).name
                  ? { ...activity, state: "awaiting_approval" }
                  : activity,
              ),
              runPhase: state.runPhase === "pending" ? "running" : state.runPhase,
            }));
          } else {
            set({ lastError: "runtime returned a malformed approval request", runPhase: "failed" });
          }
          return;
        }
        if (method === "agent.completed") {
          set({ activeRunId: null, runPhase: "completed", lastError: null });
          return;
        }
        if (method === "terminal.output") {
          // Delivered by subscription: this is a high-rate stream that must not
          // be buffered in store state.
          const data = params.data;
          if (typeof data === "string" && data) {
            for (const listener of terminalOutputListeners) {
              listener(data);
            }
          }
          return;
        }
        if (method === "terminal.exited") {
          const terminalId = params.terminal_id;
          if (typeof terminalId !== "string") return;
          const exitCode = typeof params.exit_code === "number" ? params.exit_code : null;
          const reason = params.reason === "closed" ? ("closed" as const) : ("exited" as const);
          const exit: TerminalExit = { terminalId, exitCode, reason };
          set((state) =>
            state.terminal?.id === terminalId ? { terminal: null, terminalExit: exit } : {},
          );
          for (const listener of terminalExitListeners) {
            listener(exit);
          }
          return;
        }
        if (method === "agent.failed") {
          const error = params.error;
          const code = error && typeof error === "object" && "code" in error ? String(error.code) : "runtime_error";
          const messageText =
            error && typeof error === "object" && "message" in error ? String(error.message) : "agent run failed";
          set({ activeRunId: null, runPhase: code === "cancelled" ? "cancelled" : "failed", lastError: messageText });
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
