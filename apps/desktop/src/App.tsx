import { Suspense, lazy, useEffect, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import {
  Activity,
  Bot,
  Check,
  CheckCircle2,
  ChevronRight,
  CircleAlert,
  CircleDot,
  Clock3,
  Code2,
  FileDiff,
  FolderOpen,
  GitBranch,
  History,
  LoaderCircle,
  MessageSquare,
  PanelRight,
  Play,
  PlugZap,
  RotateCcw,
  Send,
  Server,
  ShieldCheck,
  Square,
  Terminal,
  Wrench,
  X,
  XCircle,
} from "lucide-react";
import {
  startEventPump,
  useDesktopStore,
  type ChatMessage,
  type RunPhase,
  type TimelineEntry,
  type ToolActivity,
  type VerificationActivity,
} from "./store";
import { CheckpointTimeline } from "./components/checkpoint-timeline";
import type { CheckpointEntry } from "./lib/changes";
import type { GitStatusSummary, SessionSummary } from "./lib/rpc";

// Monaco and its language tokenizers are a large bundle that are only needed
// once the user opens the code view, so the changes panel loads on demand and
// the shell stays responsive on first paint.
const ChangesPanel = lazy(() =>
  import("./components/changes-panel").then((module) => ({ default: module.ChangesPanel })),
);

type CenterView = "conversation" | "changes";

function App() {
  const [address, setAddress] = useState("127.0.0.1:4545");
  const [showContext, setShowContext] = useState(true);
  const [centerView, setCenterView] = useState<CenterView>("conversation");
  const {
    status,
    clientId,
    workspacePath,
    workspace,
    sessions,
    activeSessionId,
    runPhase,
    messages,
    toolActivity,
    verificationActivity,
    timeline,
    approvals,
    composer,
    isLoadingSession,
    lastError,
    gitStatus,
    changes,
    selectedPath,
    fileChange,
    fileView,
    isLoadingChanges,
    isChangesTruncated,
    isLoadingFile,
    checkpoints,
    restoringCheckpointId,
    lastRestore,
    connect,
    setWorkspacePath,
    setComposer,
    selectSession,
    createSession,
    resumeSession,
    sendMessage,
    approve,
    deny,
    cancel,
    refreshChanges,
    selectFile,
    clearSelectedFile,
    restoreCheckpoint,
    handleServerMessage,
    setRuntimeError,
    markDisconnected,
    clearError,
  } = useDesktopStore();

  useEffect(() => {
    if (!clientId || status !== "connected") return;
    return startEventPump(
      clientId,
      handleServerMessage,
      (error) => {
        setRuntimeError(error instanceof Error ? error.message : String(error));
        markDisconnected();
      },
    );
  }, [clientId, status, handleServerMessage, markDisconnected, setRuntimeError]);

  const connected = status === "connected";
  const running = runPhase === "pending" || runPhase === "running" || runPhase === "cancelling";
  const activeSession = sessions.find((session) => session.id === activeSessionId);

  async function chooseWorkspace() {
    const selected = await open({ directory: true, multiple: false, title: "Select workspace" });
    if (typeof selected === "string") setWorkspacePath(selected);
  }

  async function connectRuntime() {
    await connect(address, workspacePath);
  }

  async function submitMessage(event: React.FormEvent) {
    event.preventDefault();
    if (composer.trim()) await sendMessage(composer);
  }

  return (
    <div className="flex h-screen min-h-[640px] flex-col bg-ink-950 text-ink-200">
      <TopBar
        status={status}
        address={address}
        workspacePath={workspacePath}
        connected={connected}
        onAddressChange={setAddress}
        onWorkspaceChange={setWorkspacePath}
        onChooseWorkspace={chooseWorkspace}
        onConnect={connectRuntime}
        onToggleContext={() => setShowContext((value) => !value)}
        showContext={showContext}
      />
      <div className="flex min-h-0 flex-1">
        <Sidebar
          sessions={sessions}
          activeSessionId={activeSessionId}
          onSelect={selectSession}
          onNew={createSession}
          disabled={!connected || running || isLoadingSession}
        />
        <main className="flex min-w-0 flex-1 flex-col">
          <WorkspaceHeader
            workspace={workspace}
            session={activeSession}
            runPhase={runPhase}
            running={running}
            isLoadingSession={isLoadingSession}
            canResume={connected && Boolean(activeSessionId)}
            onResume={() => void resumeSession()}
            onCancel={cancel}
            gitStatus={gitStatus}
            changeCount={changes.entries.length}
          />
          <ViewSwitcher
        view={centerView}
        onChange={setCenterView}
        changeCount={changes.entries.length}
        canRefresh={connected}
        isRefreshing={isLoadingChanges}
        onRefresh={() => void refreshChanges()}
      />
          <div className="flex min-h-0 flex-1">
            {centerView === "conversation" ? (
              <section className="flex min-w-0 flex-1 flex-col">
                <MessageStream messages={messages} connected={connected} runPhase={runPhase} />
                <Composer
                  value={composer}
                  onChange={setComposer}
                  onSubmit={submitMessage}
                  disabled={!connected || running || !workspacePath}
                  running={running}
                  onCancel={cancel}
                />
              </section>
            ) : (
              <Suspense fallback={<PanelLoading label="Loading code view…" />}>
                <ChangesPanel
                  entries={changes.entries}
                  selectedPath={selectedPath}
                  fileChange={fileChange}
                  fileView={fileView}
                  isLoading={isLoadingChanges || isLoadingFile}
                  isTruncated={isChangesTruncated}
                  totalChanged={gitStatus?.changed_files.length ?? changes.entries.length}
                  isGitWorkspace={Boolean(workspace?.repository_root)}
                  onSelect={(path) => void selectFile(path)}
                  onClear={clearSelectedFile}
                />
              </Suspense>
            )}
            {showContext ? (
              <ContextPanel
                tools={toolActivity}
                verification={verificationActivity}
                timeline={timeline}
                approvals={approvals}
                onApprove={approve}
                onDeny={deny}
                checkpoints={checkpoints}
                restoringId={restoringCheckpointId}
                lastRestore={lastRestore}
                restoreDisabled={!connected || running}
                onRestore={(id) => void restoreCheckpoint(id)}
              />
            ) : null}
          </div>
        </main>
      </div>
      <StatusBar status={status} runPhase={runPhase} workspacePath={workspacePath} lastError={lastError} onClearError={clearError} />
      {lastError ? <ErrorToast message={lastError} onClose={clearError} /> : null}
    </div>
  );
}

function ViewSwitcher({
  view,
  onChange,
  changeCount,
  canRefresh,
  isRefreshing,
  onRefresh,
}: {
  view: CenterView;
  onChange: (view: CenterView) => void;
  changeCount: number;
  canRefresh: boolean;
  isRefreshing: boolean;
  onRefresh: () => void;
}) {
  return (
    <div className="flex shrink-0 items-center gap-1 border-b border-ink-800 bg-ink-950/60 px-4 py-1.5" role="tablist" aria-label="Workspace view">
      <button
        role="tab"
        aria-selected={view === "conversation"}
        className={`flex items-center gap-1.5 rounded-md px-2.5 py-1.5 text-[11px] font-medium transition-colors ${
          view === "conversation" ? "bg-ink-800 text-ink-100" : "text-ink-400 hover:bg-ink-850 hover:text-ink-200"
        }`}
        onClick={() => onChange("conversation")}
      >
        <MessageSquare size={12} /> Conversation
      </button>
      <button
        role="tab"
        aria-selected={view === "changes"}
        className={`flex items-center gap-1.5 rounded-md px-2.5 py-1.5 text-[11px] font-medium transition-colors ${
          view === "changes" ? "bg-ink-800 text-ink-100" : "text-ink-400 hover:bg-ink-850 hover:text-ink-200"
        }`}
        onClick={() => onChange("changes")}
      >
        <FileDiff size={12} /> Changes
        {changeCount > 0 ? (
          <span className="rounded bg-ink-700 px-1.5 py-0.5 font-mono text-[9px] text-ink-200">{changeCount}</span>
        ) : null}
      </button>
      {view === "changes" ? (
        <button
          className="icon-button ml-auto h-6 w-6"
          onClick={onRefresh}
          disabled={!canRefresh || isRefreshing}
          aria-label="Refresh code changes"
          title="Re-read code changes from the runtime"
        >
          <RotateCcw size={12} className={isRefreshing ? "animate-spin" : ""} />
        </button>
      ) : null}
    </div>
  );
}


function TopBar({
  status,
  address,
  workspacePath,
  connected,
  onAddressChange,
  onWorkspaceChange,
  onChooseWorkspace,
  onConnect,
  onToggleContext,
  showContext,
}: {
  status: string;
  address: string;
  workspacePath: string;
  connected: boolean;
  onAddressChange: (value: string) => void;
  onWorkspaceChange: (value: string) => void;
  onChooseWorkspace: () => void;
  onConnect: () => void;
  onToggleContext: () => void;
  showContext: boolean;
}) {
  return (
    <header className="flex h-14 shrink-0 items-center gap-3 border-b border-ink-800 bg-ink-950/95 px-4">
      <div className="flex w-56 shrink-0 items-center gap-2">
        <div className="flex h-7 w-7 items-center justify-center rounded-md bg-signal-500 text-ink-950">
          <Code2 size={16} strokeWidth={2.4} />
        </div>
        <span className="text-sm font-semibold tracking-tight text-ink-100">CogitoAI</span>
        <span className="mono-label ml-1">v0</span>
      </div>
      <div className="flex min-w-0 flex-1 items-center gap-2">
        <div className="flex min-w-0 flex-1 items-center gap-2 rounded-md border border-ink-800 bg-ink-900 px-2.5">
          <Server size={14} className="shrink-0 text-ink-500" />
          <input
            aria-label="Runtime address"
            className="min-w-0 flex-1 bg-transparent py-2 font-mono text-xs text-ink-200 outline-none placeholder:text-ink-600"
            value={address}
            onChange={(event) => onAddressChange(event.target.value)}
            placeholder="127.0.0.1:4545"
          />
          <StatusPill status={status} />
        </div>
        <div className="flex min-w-0 flex-1 items-center gap-2 rounded-md border border-ink-800 bg-ink-900 px-2.5">
          <FolderOpen size={14} className="shrink-0 text-ink-500" />
          <input
            aria-label="Workspace path"
            className="min-w-0 flex-1 bg-transparent py-2 font-mono text-xs text-ink-200 outline-none placeholder:text-ink-600"
            value={workspacePath}
            onChange={(event) => onWorkspaceChange(event.target.value)}
            placeholder="Choose a repository path"
          />
          <button className="icon-button" onClick={onChooseWorkspace} aria-label="Choose workspace" title="Choose workspace">
            <FolderOpen size={15} />
          </button>
        </div>
        <button className="primary-button" onClick={onConnect} disabled={connected || !workspacePath}>
          {status === "connecting" ? <LoaderCircle size={14} className="animate-spin" /> : <PlugZap size={14} />}
          {connected ? "Connected" : "Connect"}
        </button>
      </div>
      <button
        className={`icon-button ${showContext ? "bg-ink-800 text-ink-100" : ""}`}
        onClick={onToggleContext}
        aria-label="Toggle context panel"
        title="Toggle context panel"
      >
        <PanelRight size={16} />
      </button>
    </header>
  );
}

function StatusPill({ status }: { status: string }) {
  const connected = status === "connected";
  const busy = status === "connecting";
  return (
    <span className={`flex shrink-0 items-center gap-1.5 text-[10px] font-medium ${connected ? "text-success" : busy ? "text-warning" : "text-ink-500"}`}>
      <CircleDot size={11} className={busy ? "animate-pulse" : ""} />
      {status}
    </span>
  );
}

function Sidebar({
  sessions,
  activeSessionId,
  onSelect,
  onNew,
  disabled,
}: {
  sessions: SessionSummary[];
  activeSessionId: string | null;
  onSelect: (id: string) => void;
  onNew: () => void;
  disabled: boolean;
}) {
  return (
    <aside className="flex w-64 shrink-0 flex-col border-r border-ink-800 bg-ink-900/60">
      <div className="flex items-center justify-between border-b border-ink-800 px-4 py-3">
        <div>
          <p className="mono-label">Workspace</p>
          <p className="mt-1 max-w-48 truncate text-xs text-ink-300">Sessions</p>
        </div>
        <button className="icon-button" onClick={() => void onNew()} disabled={disabled} aria-label="New session" title="New session">
          <Play size={15} />
        </button>
      </div>
      <nav className="min-h-0 flex-1 overflow-y-auto p-2" aria-label="Sessions">
        {sessions.length === 0 ? (
          <div className="px-3 py-8 text-center text-xs leading-5 text-ink-500">
            No sessions yet. Connect a runtime and start a task.
          </div>
        ) : (
          sessions.map((session) => (
            <button
              key={session.id}
              className={`group mb-1 flex w-full items-start gap-2 rounded-md px-2.5 py-2 text-left transition-colors ${session.id === activeSessionId ? "bg-ink-800 text-ink-100" : "text-ink-400 hover:bg-ink-850 hover:text-ink-200"}`}
              onClick={() => onSelect(session.id)}
            >
              <MessageSquare size={14} className="mt-0.5 shrink-0 text-ink-500" />
              <span className="min-w-0 flex-1">
                <span className="block truncate font-mono text-[11px]">{session.id}</span>
                <span className="mt-1 flex items-center gap-2 text-[10px] text-ink-500">
                  <span className={session.status === "Failed" ? "text-danger" : session.status === "Completed" ? "text-success" : "text-signal-400"}>
                    {session.status}
                  </span>
                  <span>{session.event_count} events</span>
                </span>
              </span>
              <ChevronRight size={13} className="mt-1 shrink-0 text-ink-600 group-hover:text-ink-400" />
            </button>
          ))
        )}
      </nav>
      <div className="border-t border-ink-800 p-3">
        <div className="flex items-center gap-2 text-[11px] text-ink-500">
          <History size={13} />
          <span>Durable JSONL history</span>
        </div>
      </div>
    </aside>
  );
}

function WorkspaceHeader({
  workspace,
  session,
  runPhase,
  running,
  isLoadingSession,
  canResume,
  onResume,
  onCancel,
  gitStatus,
  changeCount,
}: {
  workspace: { current_directory: string; repository_root: string | null; languages: string[] } | null;
  session: SessionSummary | undefined;
  runPhase: RunPhase;
  running: boolean;
  isLoadingSession: boolean;
  canResume: boolean;
  onResume: () => void;
  onCancel: () => void;
  gitStatus: GitStatusSummary | null;
  changeCount: number;
}) {
  return (
    <div className="flex h-16 shrink-0 items-center justify-between border-b border-ink-800 px-6">
      <div className="min-w-0">
        <div className="flex items-center gap-2">
          <h1 className="truncate text-sm font-semibold text-ink-100">{workspace?.repository_root ?? workspace?.current_directory ?? "No workspace selected"}</h1>
          <RunBadge phase={runPhase} />
        </div>
        <div className="mt-1 flex items-center gap-3 text-[11px] text-ink-500">
          <span className="flex items-center gap-1">
            <GitBranch size={12} />
            {gitStatus?.branch ? gitStatus.branch : workspace?.repository_root ? "git workspace" : "local folder"}
          </span>
          <span>{workspace?.languages?.length ?? 0} languages</span>
          <span>{session ? `${session.event_count} events` : "new session"}</span>
          {changeCount > 0 ? <span className="text-signal-400">{changeCount} changed files</span> : null}
        </div>
      </div>
      <div className="flex items-center gap-2">
        {canResume && !running ? (
          <button className="quiet-button" onClick={onResume} disabled={isLoadingSession}>
            {isLoadingSession ? <LoaderCircle size={13} className="animate-spin" /> : <RotateCcw size={13} />}
            Resume
          </button>
        ) : null}
        {running ? (
          <button className="quiet-button border-danger/30 text-danger hover:border-danger/50 hover:bg-danger/10" onClick={onCancel} disabled={runPhase === "cancelling"}>
            <Square size={13} fill="currentColor" /> {runPhase === "cancelling" ? "Cancelling" : "Stop run"}
          </button>
        ) : null}
      </div>
    </div>
  );
}

function RunBadge({ phase }: { phase: RunPhase }) {
  if (phase === "idle") return null;
  const styles: Record<Exclude<RunPhase, "idle">, string> = {
    pending: "text-warning",
    running: "text-signal-400",
    cancelling: "text-warning",
    completed: "text-success",
    failed: "text-danger",
    cancelled: "text-warning",
  };
  return (
    <span className={`flex items-center gap-1.5 text-[10px] ${styles[phase]}`}>
      {phase === "running" || phase === "pending" || phase === "cancelling" ? <Activity size={12} className="animate-pulse" /> : phase === "completed" ? <CheckCircle2 size={12} /> : <CircleAlert size={12} />}
      {phase}
    </span>
  );
}

function MessageStream({ messages, connected, runPhase }: { messages: ChatMessage[]; connected: boolean; runPhase: RunPhase }) {
  return (
    <div className="min-h-0 flex-1 overflow-y-auto px-6 py-6">
      {messages.length === 0 ? (
        <EmptyState connected={connected} />
      ) : (
        <div className="mx-auto max-w-3xl space-y-5">
          {messages.map((message) => <MessageBubble key={message.id} message={message} />)}
          {runPhase === "pending" ? <div className="flex items-center gap-2 text-xs text-ink-500"><LoaderCircle size={13} className="animate-spin" />Runtime accepted the message…</div> : null}
        </div>
      )}
    </div>
  );
}

function EmptyState({ connected }: { connected: boolean }) {
  return (
    <div className="mx-auto flex h-full max-w-lg flex-col items-center justify-center text-center">
      <div className="mb-4 flex h-12 w-12 items-center justify-center rounded-xl border border-ink-700 bg-ink-900 text-signal-400 shadow-panel">
        <Bot size={22} />
      </div>
      <h2 className="text-base font-semibold text-ink-100">Ready when the runtime is</h2>
      <p className="mt-2 max-w-sm text-sm leading-6 text-ink-500">
        {connected ? "Send a task to start a durable session. Tool approvals and runtime events will appear here." : "Connect to a running CogitoAI RPC runtime to begin. The desktop shell never runs agent logic itself."}
      </p>
    </div>
  );
}

function MessageBubble({ message }: { message: ChatMessage }) {
  const isUser = message.role === "user";
  return (
    <div className={`flex gap-3 ${isUser ? "justify-end" : "justify-start"}`}>
      {!isUser ? <div className="mt-1 flex h-6 w-6 shrink-0 items-center justify-center rounded-md bg-ink-800 text-signal-400"><Bot size={13} /></div> : null}
      <div className={`max-w-[82%] rounded-lg px-3.5 py-3 text-sm leading-6 ${isUser ? "bg-signal-500/15 text-ink-100 ring-1 ring-inset ring-signal-500/20" : "bg-ink-900 text-ink-200 ring-1 ring-inset ring-ink-800"}`}>
        <p className="whitespace-pre-wrap break-words">{message.text || (message.streaming ? "…" : "")}</p>
        {message.streaming ? <span className="ml-1 inline-block h-4 w-1.5 animate-pulse bg-signal-400 align-middle" /> : null}
      </div>
    </div>
  );
}

function Composer({
  value,
  onChange,
  onSubmit,
  disabled,
  running,
  onCancel,
}: {
  value: string;
  onChange: (value: string) => void;
  onSubmit: (event: React.FormEvent) => void;
  disabled: boolean;
  running: boolean;
  onCancel: () => void;
}) {
  return (
    <form onSubmit={onSubmit} className="shrink-0 border-t border-ink-800 bg-ink-950/80 px-6 py-4">
      <div className="mx-auto max-w-3xl rounded-lg border border-ink-700 bg-ink-900 p-2 shadow-panel focus-within:border-ink-600">
        <textarea
          aria-label="Message the agent"
          className="min-h-16 w-full resize-none bg-transparent px-2 py-1 text-sm leading-6 text-ink-100 outline-none placeholder:text-ink-600"
          placeholder={disabled ? "Connect a runtime to send a message" : "Ask the agent to inspect, change, or verify this workspace…"}
          value={value}
          onChange={(event) => onChange(event.target.value)}
          onKeyDown={(event) => {
            if (event.key === "Enter" && (event.metaKey || event.ctrlKey)) void onSubmit(event);
          }}
          disabled={disabled}
        />
        <div className="flex items-center justify-between px-1 pt-1">
          <span className="text-[10px] text-ink-600">⌘↵ to send · runtime events stream on the right</span>
          {running ? (
            <button type="button" className="quiet-button border-danger/30 text-danger" onClick={onCancel}>
              <Square size={12} fill="currentColor" /> Stop
            </button>
          ) : (
            <button type="submit" className="primary-button" disabled={disabled || !value.trim()}>
              <Send size={13} /> Send
            </button>
          )}
        </div>
      </div>
    </form>
  );
}

function ContextPanel({
  tools,
  verification,
  timeline,
  approvals,
  onApprove,
  onDeny,
  checkpoints,
  restoringId,
  lastRestore,
  restoreDisabled,
  onRestore,
}: {
  tools: ToolActivity[];
  verification: VerificationActivity[];
  timeline: TimelineEntry[];
  approvals: { approval_id: string; tool: { name: string; arguments: Record<string, unknown> } }[];
  onApprove: (id: string) => void;
  onDeny: (id: string) => void;
  checkpoints: CheckpointEntry[];
  restoringId: string | null;
  lastRestore: { checkpoint_id: string; restored_files: string[]; conflicts: string[] } | null;
  restoreDisabled: boolean;
  onRestore: (id: string) => void;
}) {
  return (
    <aside className="hidden w-96 shrink-0 flex-col border-l border-ink-800 bg-ink-900/50 xl:flex">
      <div className="flex h-16 shrink-0 items-center justify-between border-b border-ink-800 px-4">
        <div className="flex items-center gap-2"><PanelRight size={15} className="text-ink-500" /><span className="text-xs font-semibold text-ink-200">Runtime context</span></div>
        <span className="mono-label">live</span>
      </div>
      <div className="min-h-0 flex-1 overflow-y-auto">
        <div className="space-y-5 p-3">
          {approvals.length > 0 ? (
            <section className="space-y-2">
              <p className="mono-label text-warning">Needs approval</p>
              {approvals.map((approval) => (
                <div key={approval.approval_id} className="rounded-lg border border-warning/30 bg-warning/5 p-3 shadow-panel">
                  <div className="flex items-center justify-between gap-2">
                    <div className="flex items-center gap-2 text-xs font-semibold text-ink-100"><ShieldCheck size={14} className="text-warning" />{approval.tool.name}</div>
                    <span className="mono-label">allow once</span>
                  </div>
                  <p className="mt-2 text-[11px] leading-4 text-ink-400">The runtime is waiting for a one-time decision. No permanent policy change will be made.</p>
                  <pre className="mt-2 max-h-28 overflow-auto whitespace-pre-wrap break-words rounded-md bg-ink-950/60 p-2 font-mono text-[10px] leading-4 text-ink-400">{JSON.stringify(approval.tool.arguments, null, 2)}</pre>
                  <div className="mt-3 flex gap-2">
                    <button className="primary-button flex-1 justify-center" onClick={() => onApprove(approval.approval_id)}><Check size={13} />Allow once</button>
                    <button className="quiet-button flex-1 justify-center border-danger/30 text-danger" onClick={() => onDeny(approval.approval_id)}><X size={13} />Deny once</button>
                  </div>
                </div>
              ))}
            </section>
          ) : null}

          <section className="flex min-h-0 flex-col">
            <CheckpointTimeline
              checkpoints={checkpoints}
              restoringId={restoringId}
              lastRestore={lastRestore}
              disabled={restoreDisabled}
              onRestore={onRestore}
            />
          </section>

          <section>
            <div className="mb-2 flex items-center justify-between"><p className="mono-label">Tool activity</p><span className="text-[10px] text-ink-600">{tools.length} calls</span></div>
            {tools.length === 0 ? <EmptyContext text="Tool calls will appear here." /> : <div className="space-y-1.5">{[...tools].reverse().map((tool) => <ToolActivityCard key={tool.id} tool={tool} />)}</div>}
          </section>

          <section>
            <div className="mb-2 flex items-center justify-between"><p className="mono-label">Verification</p><span className="text-[10px] text-ink-600">{verification.length} checks</span></div>
            {verification.length === 0 ? <EmptyContext text="Verification results will appear here." /> : <div className="space-y-1.5">{[...verification].reverse().map((item) => <VerificationCard key={item.id} item={item} />)}</div>}
          </section>

          <section>
            <div className="mb-2 flex items-center justify-between"><p className="mono-label">Session timeline</p><span className="text-[10px] text-ink-600">chronological</span></div>
            {timeline.length === 0 ? <EmptyContext text="Session events will appear here." /> : <div className="space-y-0.5">{timeline.map((entry) => <TimelineRow key={entry.id} entry={entry} />)}</div>}
          </section>
        </div>
      </div>
    </aside>
  );
}

function EmptyContext({ text }: { text: string }) {
  return <p className="rounded-md border border-dashed border-ink-800 p-3 text-xs leading-5 text-ink-600">{text}</p>;
}

function ToolActivityCard({ tool }: { tool: ToolActivity }) {
  const tone = tool.state === "succeeded" ? "text-success" : tool.state === "failed" || tool.state === "denied" ? "text-danger" : tool.state === "running" ? "text-signal-400" : "text-ink-400";
  return (
    <details className="group rounded-lg border border-ink-800 bg-ink-900/80 open:border-ink-700">
      <summary className="flex cursor-pointer list-none items-center gap-2 px-2.5 py-2 text-[11px]">
        <Wrench size={12} className={tone} />
        <span className="min-w-0 flex-1 truncate font-medium text-ink-200">{tool.name}</span>
        <span className="truncate font-mono text-[10px] text-ink-600">{tool.target || "no target"}</span>
        {tool.durationMs !== null ? <span className="flex items-center gap-1 text-[10px] text-ink-500"><Clock3 size={10} />{formatDuration(tool.durationMs)}</span> : null}
        <span className={`text-[10px] ${tone}`}>{tool.state}</span>
        <ChevronRight size={12} className="text-ink-600 transition-transform group-open:rotate-90" />
      </summary>
      <div className="border-t border-ink-800 px-2.5 py-2">
        {tool.error ? <p className="mb-2 text-[11px] text-danger">{tool.error}</p> : null}
        <pre className="max-h-40 overflow-auto whitespace-pre-wrap break-words font-mono text-[10px] leading-4 text-ink-500">{tool.output || "No output was reported."}</pre>
      </div>
    </details>
  );
}

function VerificationCard({ item }: { item: VerificationActivity }) {
  const tone = item.state === "passed" ? "text-success" : item.state === "failed" ? "text-danger" : "text-signal-400";
  return (
    <details className="group rounded-lg border border-ink-800 bg-ink-900/80 open:border-ink-700">
      <summary className="flex cursor-pointer list-none items-center gap-2 px-2.5 py-2 text-[11px]">
        {item.state === "passed" ? <CheckCircle2 size={12} className={tone} /> : <CircleDot size={12} className={`${tone} ${item.state === "running" ? "animate-pulse" : ""}`} />}
        <span className="min-w-0 flex-1 truncate font-medium text-ink-200">{item.category}</span>
        {item.durationMs !== null ? <span className="text-[10px] text-ink-500">{formatDuration(item.durationMs)}</span> : null}
        <span className={`text-[10px] ${tone}`}>{item.state}</span>
        <ChevronRight size={12} className="text-ink-600 transition-transform group-open:rotate-90" />
      </summary>
      <div className="border-t border-ink-800 px-2.5 py-2">
        <p className="break-words font-mono text-[10px] text-ink-400">{item.command}</p>
        {item.diagnostics.length > 0 ? <pre className="mt-2 max-h-32 overflow-auto whitespace-pre-wrap font-mono text-[10px] leading-4 text-danger">{item.diagnostics.join("\n")}</pre> : null}
      </div>
    </details>
  );
}

function TimelineRow({ entry }: { entry: TimelineEntry }) {
  const tone = entry.tone === "success" ? "text-success" : entry.tone === "danger" ? "text-danger" : entry.tone === "warning" ? "text-warning" : "text-signal-400";
  return (
    <div className="grid grid-cols-[14px_1fr] gap-2 rounded-md px-2 py-1.5 text-[11px] leading-4 hover:bg-ink-850">
      {entry.tone === "danger" ? <XCircle size={12} className={tone} /> : entry.tone === "success" ? <CheckCircle2 size={12} className={tone} /> : <CircleDot size={12} className={tone} />}
      <div className="min-w-0"><p className="truncate font-mono text-ink-300">{entry.eventType}</p>{entry.detail ? <p className="truncate text-ink-600">{entry.detail}</p> : null}</div>
    </div>
  );
}

function formatDuration(durationMs: number): string {
  if (durationMs < 1000) return `${durationMs}ms`;
  return `${(durationMs / 1000).toFixed(1)}s`;
}

function PanelLoading({ label }: { label: string }) {
  return (
    <div className="flex min-h-0 flex-1 items-center justify-center gap-2 text-[11px] text-ink-500">
      <LoaderCircle size={13} className="animate-spin" />
      {label}
    </div>
  );
}

function StatusBar({ status, runPhase, workspacePath, lastError, onClearError }: { status: string; runPhase: RunPhase; workspacePath: string; lastError: string | null; onClearError: () => void }) {
  return (
    <footer className="flex h-7 shrink-0 items-center gap-4 border-t border-ink-800 bg-ink-950 px-4 text-[10px] text-ink-500">
      <span className="flex items-center gap-1.5"><span className={`h-1.5 w-1.5 rounded-full ${status === "connected" ? "bg-success" : status === "connecting" ? "bg-warning" : status === "error" ? "bg-danger" : "bg-ink-600"}`} />runtime {status}</span>
      <span className="flex items-center gap-1.5"><Terminal size={11} />{workspacePath || "no workspace"}</span>
      <span className="flex items-center gap-1.5"><Activity size={11} />run {runPhase}</span>
      <span className="ml-auto">v0 shell · runtime owns execution</span>
      {lastError ? <button className="text-danger hover:text-ink-200" onClick={onClearError}>dismiss error</button> : null}
    </footer>
  );
}

function ErrorToast({ message, onClose }: { message: string; onClose: () => void }) {
  return (
    <div className="fixed bottom-10 right-5 z-10 flex max-w-sm items-start gap-3 rounded-lg border border-danger/30 bg-ink-900 px-3 py-3 text-xs text-ink-200 shadow-panel">
      <CircleAlert size={15} className="mt-0.5 shrink-0 text-danger" />
      <p className="flex-1 leading-5">{message}</p>
      <button className="icon-button -mr-1 -mt-1 h-6 w-6" onClick={onClose} aria-label="Dismiss error"><X size={13} /></button>
    </div>
  );
}

export default App;
