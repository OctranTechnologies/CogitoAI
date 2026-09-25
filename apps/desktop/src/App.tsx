import { useEffect, useMemo, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import {
  Activity,
  Bot,
  Check,
  CheckCircle2,
  ChevronRight,
  CircleAlert,
  CircleDot,
  Code2,
  FolderOpen,
  GitBranch,
  History,
  LoaderCircle,
  MessageSquare,
  PanelRight,
  Play,
  PlugZap,
  Send,
  Server,
  ShieldCheck,
  Square,
  Terminal,
  X,
  XCircle,
} from "lucide-react";
import { startEventPump, useDesktopStore, type ChatMessage } from "./store";
import type { HarnessEvent, SessionSummary } from "./lib/rpc";

function App() {
  const [address, setAddress] = useState("127.0.0.1:4545");
  const [showContext, setShowContext] = useState(true);
  const {
    status,
    clientId,
    workspacePath,
    workspace,
    sessions,
    activeSessionId,
    activeRunId,
    messages,
    events,
    approvals,
    composer,
    lastError,
    connect,
    setWorkspacePath,
    setComposer,
    selectSession,
    createSession,
    sendMessage,
    approve,
    deny,
    cancel,
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
  const running = Boolean(activeRunId);
  const activeSession = sessions.find((session) => session.id === activeSessionId);
  const eventItems = useMemo(() => events.slice(-40).reverse(), [events]);

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
          disabled={!connected || running}
        />
        <main className="flex min-w-0 flex-1 flex-col">
          <WorkspaceHeader
            workspace={workspace}
            session={activeSession}
            running={running}
            onCancel={cancel}
          />
          <div className="flex min-h-0 flex-1">
            <section className="flex min-w-0 flex-1 flex-col">
              <MessageStream messages={messages} connected={connected} />
              <Composer
                value={composer}
                onChange={setComposer}
                onSubmit={submitMessage}
                disabled={!connected || running || !workspacePath}
                running={running}
                onCancel={cancel}
              />
            </section>
            {showContext ? (
              <ContextPanel
                events={eventItems}
                approvals={approvals}
                onApprove={approve}
                onDeny={deny}
              />
            ) : null}
          </div>
        </main>
      </div>
      <StatusBar status={status} workspacePath={workspacePath} lastError={lastError} onClearError={clearError} />
      {lastError ? <ErrorToast message={lastError} onClose={clearError} /> : null}
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
  running,
  onCancel,
}: {
  workspace: { current_directory: string; repository_root: string | null; languages: string[] } | null;
  session: SessionSummary | undefined;
  running: boolean;
  onCancel: () => void;
}) {
  return (
    <div className="flex h-16 shrink-0 items-center justify-between border-b border-ink-800 px-6">
      <div className="min-w-0">
        <div className="flex items-center gap-2">
          <h1 className="truncate text-sm font-semibold text-ink-100">{workspace?.repository_root ?? workspace?.current_directory ?? "No workspace selected"}</h1>
          {running ? <span className="flex items-center gap-1.5 text-[10px] text-signal-400"><Activity size={12} className="animate-pulse" />running</span> : null}
        </div>
        <div className="mt-1 flex items-center gap-3 text-[11px] text-ink-500">
          <span className="flex items-center gap-1"><GitBranch size={12} />{workspace?.repository_root ? "git workspace" : "local folder"}</span>
          <span>{workspace?.languages?.length ?? 0} languages</span>
          <span>{session ? `${session.event_count} events` : "new session"}</span>
        </div>
      </div>
      {running ? (
        <button className="quiet-button border-danger/30 text-danger hover:border-danger/50 hover:bg-danger/10" onClick={onCancel}>
          <Square size={13} fill="currentColor" /> Stop run
        </button>
      ) : null}
    </div>
  );
}

function MessageStream({ messages, connected }: { messages: ChatMessage[]; connected: boolean }) {
  return (
    <div className="min-h-0 flex-1 overflow-y-auto px-6 py-6">
      {messages.length === 0 ? (
        <EmptyState connected={connected} />
      ) : (
        <div className="mx-auto max-w-3xl space-y-5">
          {messages.map((message) => <MessageBubble key={message.id} message={message} />)}
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
  events,
  approvals,
  onApprove,
  onDeny,
}: {
  events: HarnessEvent[];
  approvals: { approval_id: string; tool: { name: string; arguments: Record<string, unknown> } }[];
  onApprove: (id: string) => void;
  onDeny: (id: string) => void;
}) {
  return (
    <aside className="hidden w-80 shrink-0 flex-col border-l border-ink-800 bg-ink-900/50 xl:flex">
      <div className="flex h-16 items-center justify-between border-b border-ink-800 px-4">
        <div className="flex items-center gap-2"><PanelRight size={15} className="text-ink-500" /><span className="text-xs font-semibold text-ink-200">Runtime context</span></div>
        <span className="mono-label">live</span>
      </div>
      <div className="min-h-0 flex-1 overflow-y-auto p-3">
        {approvals.length > 0 ? (
          <section className="mb-4 space-y-2">
            <p className="mono-label text-warning">Needs approval</p>
            {approvals.map((approval) => (
              <div key={approval.approval_id} className="rounded-lg border border-warning/30 bg-warning/5 p-3 shadow-panel">
                <div className="flex items-center gap-2 text-xs font-semibold text-ink-100"><ShieldCheck size={14} className="text-warning" />{approval.tool.name}</div>
                <pre className="mt-2 max-h-24 overflow-auto whitespace-pre-wrap break-words font-mono text-[10px] leading-4 text-ink-400">{JSON.stringify(approval.tool.arguments, null, 2)}</pre>
                <div className="mt-3 flex gap-2">
                  <button className="primary-button flex-1 justify-center" onClick={() => onApprove(approval.approval_id)}><Check size={13} />Approve</button>
                  <button className="quiet-button flex-1 justify-center border-danger/30 text-danger" onClick={() => onDeny(approval.approval_id)}><X size={13} />Deny</button>
                </div>
              </div>
            ))}
          </section>
        ) : null}
        <section>
          <div className="mb-2 flex items-center justify-between"><p className="mono-label">Event stream</p><span className="text-[10px] text-ink-600">{events.length} recent</span></div>
          {events.length === 0 ? <p className="rounded-md border border-dashed border-ink-800 p-3 text-xs leading-5 text-ink-600">Runtime events will appear here.</p> : <div className="space-y-1">{events.map((event) => <EventRow key={event.event_id} event={event} />)}</div>}
        </section>
      </div>
    </aside>
  );
}

function EventRow({ event }: { event: HarnessEvent }) {
  const data = event.payload.data;
  const detail = typeof data.text === "string" ? data.text : typeof data.tool === "string" ? data.tool : typeof data.command === "string" ? data.command : "";
  const failed = event.event_type.endsWith(".failed") || event.event_type === "tool.denied";
  return (
    <div className="flex gap-2 rounded-md px-2 py-1.5 text-[11px] leading-4 hover:bg-ink-850">
      {failed ? <XCircle size={12} className="mt-0.5 shrink-0 text-danger" /> : event.event_type.includes("completed") ? <CheckCircle2 size={12} className="mt-0.5 shrink-0 text-success" /> : <CircleDot size={12} className="mt-0.5 shrink-0 text-signal-400" />}
      <div className="min-w-0"><p className="truncate font-mono text-ink-300">{event.event_type}</p>{detail ? <p className="truncate text-ink-600">{detail}</p> : null}</div>
    </div>
  );
}

function StatusBar({ status, workspacePath, lastError, onClearError }: { status: string; workspacePath: string; lastError: string | null; onClearError: () => void }) {
  return (
    <footer className="flex h-7 shrink-0 items-center gap-4 border-t border-ink-800 bg-ink-950 px-4 text-[10px] text-ink-500">
      <span className="flex items-center gap-1.5"><span className={`h-1.5 w-1.5 rounded-full ${status === "connected" ? "bg-success" : status === "connecting" ? "bg-warning" : status === "error" ? "bg-danger" : "bg-ink-600"}`} />runtime {status}</span>
      <span className="flex items-center gap-1.5"><Terminal size={11} />{workspacePath || "no workspace"}</span>
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
