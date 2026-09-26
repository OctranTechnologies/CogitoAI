import { Suspense, lazy, useEffect, useMemo, useState } from "react";
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
  LoaderCircle,
  MessageSquare,
  PanelRight,
  Play,
  PlugZap,
  RotateCcw,
  Server,
  Settings2,
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
import { SettingsDialog } from "./components/settings-dialog";
import { TerminalPanel } from "./components/terminal-panel";
import { AppRail, type RailTarget } from "./components/app-rail";
import { ProjectSidebar, type ProjectEntry } from "./components/project-sidebar";
import { ActivityView, HistoryView, ProjectsView } from "./components/workspace-views";
import { LandingView } from "./components/landing-view";
import { PromptComposer } from "./components/prompt-composer";
import type { SettingsScreen } from "./lib/settings";
import { readStoredRailTarget, storeRailTarget } from "./lib/shell-prefs";
import {
  Badge,
  Button,
  CommandMenu,
  EmptyState,
  IconButton,
  ScrollArea,
  StatusIndicator,
  Tooltip,
  toneText,
  type Tone,
} from "./components/ui";
import { cx } from "./components/ui/cx";
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
  const [showSettings, setShowSettings] = useState(false);
  const [showCommands, setShowCommands] = useState(false);
  const [railTarget, setRailTarget] = useState<RailTarget>(readStoredRailTarget);
  const [settingsScreen, setSettingsScreen] = useState<SettingsScreen>("models");
  const [pendingMode, setPendingMode] = useState<string | null>(null);
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
    terminal,
    settings,
    updateModel,
    updatePermissionMode,
    closeTerminal,
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
  // The landing screen is for "no conversation selected". A session that exists
  // but has not been resumed yet has no transcript, so it still counts as empty.
  const hasConversation = Boolean(activeSessionId) && messages.length > 0;

  // A terminal is bound to the workspace it was opened against, so it is
  // closed whenever the workspace changes or the connection goes away rather
  // than being carried across into a different repository.
  useEffect(() => {
    if (!terminal) return;
    return () => {
      void closeTerminal();
    };
  }, [terminal, workspacePath, connected, closeTerminal]);

  // Closing the window releases the runtime connection, which makes the runtime
  // terminate this client's terminal processes instead of leaking them.
  useEffect(() => {
    const onUnload = () => {
      if (terminal) void closeTerminal();
    };
    window.addEventListener("beforeunload", onUnload);
    return () => window.removeEventListener("beforeunload", onUnload);
  }, [terminal, closeTerminal]);

  // The command palette only offers actions that already exist elsewhere in the
  // shell. It is a faster route to them, not a new capability.
  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "k") {
        event.preventDefault();
        setShowCommands((value) => !value);
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, []);

  async function chooseWorkspace() {
    const selected = await open({ directory: true, multiple: false, title: "Select workspace" });
    if (typeof selected === "string") setWorkspacePath(selected);
  }

  async function connectRuntime() {
    await connect(address, workspacePath);
  }

  /**
   * The rail's selection is the only piece of shell state that outlives a
   * restart, so it is the only one persisted.
   */
  function onRailSelect(target: RailTarget) {
    setRailTarget(target);
    storeRailTarget(target);
    if (target === "models" || target === "settings") {
      setSettingsScreen(target === "models" ? "models" : "runtime");
      setShowSettings(true);
    }
  }

  function onSelectProject(path: string) {
    if (!path) return;
    setWorkspacePath(path);
  }

  // Projects come from the sessions the runtime reports, so the list reflects
  // real workspaces rather than invented entries.
  const projects = useMemo<ProjectEntry[]>(() => {
    const counts = new Map<string, number>();
    for (const session of sessions) {
      counts.set(session.workspace_root, (counts.get(session.workspace_root) ?? 0) + 1);
    }
    const roots = new Set<string>(counts.keys());
    if (workspacePath) roots.add(workspacePath);
    return [...roots].map((path) => ({
      path,
      label: path.split(/[\\/]/).filter(Boolean).pop() ?? path,
      sessionCount: counts.get(path) ?? 0,
      active: path === workspacePath,
    }));
  }, [sessions, workspacePath]);


  /**
   * The composer owns submission so that Enter, Shift+Enter, and the send
   * button all funnel through the same path; the form event is only used to stop
   * the browser navigating on a native submit.
   */
  async function submitPrompt(text: string) {
    await sendMessage(text);
  }

  async function selectModel(model: string) {
    await updateModel({ model });
  }

  async function selectMode(mode: string) {
    if (mode === settings?.permissions.mode) return;
    setPendingMode(mode);
    try {
      await updatePermissionMode(mode);
    } finally {
      setPendingMode(null);
    }
  }

  // The palette only offers actions that already exist elsewhere in the shell.
  // It is a faster route to them, not a new capability, and it is built inline
  // because the handlers it closes over are recreated on every render.
  const commandItems = [
    {
      id: "connect",
      label: connected ? "Runtime connected" : "Connect to runtime",
      detail: address,
      icon: <PlugZap className="size-icon-sm" />,
      disabled: connected || !workspacePath,
      onSelect: () => void connectRuntime(),
    },
    {
      id: "workspace",
      label: "Choose workspace folder",
      icon: <FolderOpen className="size-icon-sm" />,
      onSelect: () => void chooseWorkspace(),
    },
    {
      id: "session.new",
      label: "New session",
      icon: <Play className="size-icon-sm" />,
      disabled: !connected || running || isLoadingSession,
      onSelect: () => void createSession(),
    },
    {
      id: "session.resume",
      label: "Resume selected session",
      icon: <RotateCcw className="size-icon-sm" />,
      disabled: !connected || !activeSessionId || running,
      onSelect: () => void resumeSession(),
    },
    {
      id: "view.changes",
      label: "Show code changes",
      icon: <FileDiff className="size-icon-sm" />,
      onSelect: () => setCenterView("changes"),
    },
    {
      id: "view.conversation",
      label: "Show conversation",
      icon: <MessageSquare className="size-icon-sm" />,
      onSelect: () => setCenterView("conversation"),
    },
    {
      id: "changes.refresh",
      label: "Refresh code changes",
      icon: <RotateCcw className="size-icon-sm" />,
      disabled: !connected,
      onSelect: () => void refreshChanges(),
    },
    {
      id: "context.toggle",
      label: showContext ? "Hide context panel" : "Show context panel",
      icon: <PanelRight className="size-icon-sm" />,
      onSelect: () => setShowContext((value) => !value),
    },
    {
      id: "run.cancel",
      label: "Stop the running task",
      icon: <Square className="size-icon-sm" />,
      disabled: !running,
      onSelect: () => cancel(),
    },
    {
      id: "settings",
      label: "Open settings",
      icon: <Settings2 className="size-icon-sm" />,
      onSelect: () => setShowSettings(true),
    },
  ];

  return (
    <div className="flex h-screen min-h-[560px] overflow-hidden bg-app text-secondary">
      <AppRail
        target={railTarget}
        onSelect={onRailSelect}
        profileLabel="Local runtime session"
        statusSlot={<BrandMark />}
      />
      <ProjectSidebar
        productName="CogitoAI"
        sessions={sessions}
        activeSessionId={activeSessionId}
        workspacePath={workspacePath}
        status={status}
        projects={projects}
        onSelectSession={selectSession}
        onNewSession={() => void createSession()}
        onSelectProject={onSelectProject}
        disabled={!connected || running || isLoadingSession}
        onSearch={() => setShowCommands(true)}
      />
      <main className="flex min-w-0 flex-1 flex-col">
        <WorkspaceTopBar
          status={status}
          address={address}
          connected={connected}
          onAddressChange={setAddress}
          onConnect={connectRuntime}
          onChooseWorkspace={chooseWorkspace}
          onToggleContext={() => setShowContext((value) => !value)}
          onOpenCommands={() => setShowCommands(true)}
          showContext={showContext}
        />
        {railTarget === "home" ? (
          <>
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
              <div className="flex min-w-0 flex-1 flex-col">
                <div className="flex min-h-0 flex-1">
                  {centerView === "conversation" ? (
                    <section className="flex min-w-0 flex-1 flex-col">
                      {hasConversation ? (
                        <>
                          <MessageStream
                            messages={messages}
                            connected={connected}
                            runPhase={runPhase}
                          />
                          <div className="shrink-0 border-t border-line bg-app px-6 py-4">
                            <div className="mx-auto max-w-3xl">
                              <PromptComposer
                                value={composer}
                                onChange={setComposer}
                                onSubmit={submitPrompt}
                                disabled={!connected || running || !workspacePath}
                                running={running}
                                onCancel={cancel}
                                models={settings?.models ?? null}
                                permissions={settings?.permissions ?? null}
                                onSelectModel={selectModel}
                                onSelectMode={selectMode}
                                pendingMode={pendingMode}
                                workspacePath={workspacePath}
                                onChooseWorkspace={chooseWorkspace}
                                connected={connected}
                              />
                            </div>
                          </div>
                        </>
                      ) : (
                        <LandingView
                          value={composer}
                          onChange={setComposer}
                          onSubmit={submitPrompt}
                          disabled={!connected || running || !workspacePath}
                          running={running}
                          onCancel={cancel}
                          connected={connected}
                          workspacePath={workspacePath}
                          onChooseWorkspace={chooseWorkspace}
                          models={settings?.models ?? null}
                          permissions={settings?.permissions ?? null}
                          onSelectModel={selectModel}
                          onSelectMode={selectMode}
                          pendingMode={pendingMode}
                          runtimeError={lastError}
                        />
                      )}
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
                <TerminalPanel />
              </div>
            </div>
          </>
        ) : railTarget === "history" ? (
          <HistoryView
            timeline={timeline}
            sessions={sessions}
            onSelectSession={selectSession}
            activeSessionId={activeSessionId}
          />
        ) : railTarget === "activity" ? (
          <ActivityView tools={toolActivity} verification={verificationActivity} />
        ) : (
          <ProjectsView project={settings?.project ?? null} />
        )}
        <StatusBar
          status={status}
          runPhase={runPhase}
          workspacePath={workspacePath}
          lastError={lastError}
          onClearError={clearError}
        />
      </main>
      <SettingsDialog
        open={showSettings}
        initialScreen={settingsScreen}
        onClose={() => setShowSettings(false)}
      />
      <CommandMenu
        open={showCommands}
        onClose={() => setShowCommands(false)}
        items={commandItems}
        label="Command palette"
        placeholder="Jump to an action…"
      />
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
  const tab = (id: CenterView, label: string, icon: React.ReactNode, count?: number) => (
    <button
      role="tab"
      aria-selected={view === id}
      className={cx(
        "inline-flex items-center gap-1.5 rounded-md px-2.5 py-1.5 text-xs font-medium",
        "transition-colors duration-fast",
        view === id ? "bg-active text-primary" : "text-muted hover:bg-hover hover:text-primary",
      )}
      onClick={() => onChange(id)}
    >
      {icon}
      {label}
      {count !== undefined && count > 0 ? (
        <span className="rounded-sm bg-active px-1 py-0.5 font-mono text-2xs text-secondary">
          {count}
        </span>
      ) : null}
    </button>
  );

  return (
    <div
      className="flex shrink-0 items-center gap-1 border-b border-line bg-app px-4 py-1.5"
      role="tablist"
      aria-label="Workspace view"
    >
      {tab("conversation", "Conversation", <MessageSquare className="size-icon-sm" />)}
      {tab("changes", "Changes", <FileDiff className="size-icon-sm" />, changeCount)}
      {view === "changes" ? (
        <Tooltip label="Re-read code changes from the runtime">
          <IconButton
            label="Refresh code changes"
            size="sm"
            className="ml-auto"
            onClick={onRefresh}
            disabled={!canRefresh || isRefreshing}
          >
            <RotateCcw className={cx("size-icon-sm", isRefreshing && "animate-spin")} />
          </IconButton>
        </Tooltip>
      ) : null}
    </div>
  );
}

/** Product mark for the rail. Uses the harness's own glyph, not a vendor logo. */
function BrandMark() {
  return (
    <span
      aria-hidden
      className="flex size-7 items-center justify-center rounded-md bg-accent text-inverse"
    >
      <Code2 className="size-icon-md" strokeWidth={2.4} />
    </span>
  );
}

/**
 * Connection controls for the main region.
 *
 * In a three-region layout the top bar belongs to the workspace rather than the
 * application, so it carries only what scopes the current run: which runtime is
 * connected, and the view-level toggles. Product identity and the workspace
 * switcher live in the sidebar.
 */
function WorkspaceTopBar({
  status,
  address,
  connected,
  onAddressChange,
  onConnect,
  onChooseWorkspace,
  onToggleContext,
  onOpenCommands,
  showContext,
}: {
  status: string;
  address: string;
  connected: boolean;
  onAddressChange: (value: string) => void;
  onConnect: () => void;
  onChooseWorkspace: () => void;
  onToggleContext: () => void;
  onOpenCommands: () => void;
  showContext: boolean;
}) {
  return (
    <header className="flex h-12 shrink-0 items-center gap-2 border-b border-line bg-app px-3">
      <div className="flex min-w-0 flex-1 items-center gap-2 rounded-md border border-line bg-panel px-2.5 transition-colors duration-fast focus-within:border-line-stronger">
        <Server className="size-icon-md shrink-0 text-faint" />
        <input
          aria-label="Runtime address"
          className="min-w-0 flex-1 bg-transparent py-1.5 font-mono text-xs text-secondary outline-none placeholder:text-faint"
          value={address}
          onChange={(event) => onAddressChange(event.target.value)}
          placeholder="127.0.0.1:4545"
        />
        <StatusIndicator status={status} />
      </div>

      <Tooltip label="Choose workspace folder">
        <IconButton label="Choose workspace" onClick={onChooseWorkspace}>
          <FolderOpen className="size-icon-lg" />
        </IconButton>
      </Tooltip>

      <Button
        variant="primary"
        size="sm"
        onClick={onConnect}
        disabled={connected}
        icon={
          status === "connecting" ? (
            <LoaderCircle className="size-icon-sm animate-spin" />
          ) : (
            <PlugZap className="size-icon-sm" />
          )
        }
      >
        {connected ? "Connected" : "Connect"}
      </Button>

      <Tooltip label="Command palette (Ctrl+K)">
        <IconButton label="Open command palette" onClick={onOpenCommands}>
          <Code2 className="size-icon-lg" />
        </IconButton>
      </Tooltip>
      <IconButton label="Toggle context panel" onClick={onToggleContext} active={showContext}>
        <PanelRight className="size-icon-lg" />
      </IconButton>
    </header>
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
    <div className="flex h-16 shrink-0 items-center justify-between border-b border-line px-6">
      <div className="min-w-0">
        <div className="flex items-center gap-2">
          <h1 className="truncate text-md font-semibold text-primary">
            {workspace?.repository_root ?? workspace?.current_directory ?? "No workspace selected"}
          </h1>
          <RunBadge phase={runPhase} />
        </div>
        <div className="mt-1 flex items-center gap-3 text-2xs text-faint">
          <span className="flex items-center gap-1">
            <GitBranch className="size-icon-sm" />
            {gitStatus?.branch ? gitStatus.branch : workspace?.repository_root ? "git workspace" : "local folder"}
          </span>
          <span>{workspace?.languages?.length ?? 0} languages</span>
          <span>{session ? `${session.event_count} events` : "new session"}</span>
          {changeCount > 0 ? (
            <span className="text-accent">{changeCount} changed files</span>
          ) : null}
        </div>
      </div>
      <div className="flex items-center gap-2">
        {canResume && !running ? (
          <Button
            onClick={onResume}
            disabled={isLoadingSession}
            icon={
              isLoadingSession ? (
                <LoaderCircle className="size-icon-sm animate-spin" />
              ) : (
                <RotateCcw className="size-icon-sm" />
              )
            }
          >
            Resume
          </Button>
        ) : null}
        {running ? (
          <Button
            variant="danger"
            onClick={onCancel}
            disabled={runPhase === "cancelling"}
            icon={<Square className="size-icon-sm" fill="currentColor" />}
          >
            {runPhase === "cancelling" ? "Cancelling" : "Stop run"}
          </Button>
        ) : null}
      </div>
    </div>
  );
}

function RunBadge({ phase }: { phase: RunPhase }) {
  if (phase === "idle") return null;
  const tone: Tone =
    phase === "completed"
      ? "success"
      : phase === "failed"
        ? "error"
        : phase === "running"
          ? "accent"
          : "warning";
  const busy = phase === "running" || phase === "pending" || phase === "cancelling";
  return (
    <Badge tone={tone} indicator={false}>
      {busy ? (
        <Activity className="size-icon-xs animate-pulse" />
      ) : phase === "completed" ? (
        <CheckCircle2 className="size-icon-xs" />
      ) : (
        <CircleAlert className="size-icon-xs" />
      )}
      {phase}
    </Badge>
  );
}

function MessageStream({
  messages,
  connected,
  runPhase,
}: {
  messages: ChatMessage[];
  connected: boolean;
  runPhase: RunPhase;
}) {
  return (
    <ScrollArea className="flex-1 px-6 py-6">
      {messages.length === 0 ? (
        <EmptyConversation connected={connected} />
      ) : (
        <div className="mx-auto max-w-3xl space-y-5">
          {messages.map((message) => (
            <MessageBubble key={message.id} message={message} />
          ))}
          {runPhase === "pending" ? (
            <p className="flex items-center gap-2 text-xs text-faint">
              <LoaderCircle className="size-icon-sm animate-spin" />Runtime accepted the message…
            </p>
          ) : null}
        </div>
      )}
    </ScrollArea>
  );
}

function EmptyConversation({ connected }: { connected: boolean }) {
  return (
    <div className="mx-auto flex h-full max-w-lg flex-col items-center justify-center text-center">
      <div className="mb-4 flex size-12 items-center justify-center rounded-xl border border-line bg-panel text-accent">
        <Bot className="size-icon-2xl" />
      </div>
      <h2 className="text-md font-semibold text-primary">Ready when the runtime is</h2>
      <p className="mt-2 max-w-sm text-sm leading-6 text-faint">
        {connected
          ? "Send a task to start a durable session. Tool approvals and runtime events will appear here."
          : "Connect to a running CogitoAI RPC runtime to begin. The desktop shell never runs agent logic itself."}
      </p>
    </div>
  );
}

function MessageBubble({ message }: { message: ChatMessage }) {
  const isUser = message.role === "user";
  return (
    <div className={cx("flex gap-3", isUser ? "justify-end" : "justify-start")}>
      {!isUser ? (
        <div className="mt-1 flex size-control-sm shrink-0 items-center justify-center rounded-md bg-elevated text-accent">
          <Bot className="size-icon-sm" />
        </div>
      ) : null}
      <div
        className={cx(
          "max-w-[82%] rounded-lg px-3.5 py-3 text-sm leading-6",
          isUser
            ? "bg-accent/10 text-primary ring-1 ring-inset ring-accent/25"
            : "bg-panel text-secondary ring-1 ring-inset ring-line",
        )}
      >
        <p className="whitespace-pre-wrap break-words">
          {message.text || (message.streaming ? "…" : "")}
        </p>
        {message.streaming ? (
          <span className="ml-1 inline-block h-4 w-1.5 animate-pulse bg-accent align-middle" />
        ) : null}
      </div>
    </div>
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
    <aside className="hidden w-96 shrink-0 flex-col border-l border-line bg-panel xl:flex">
      <div className="flex h-16 shrink-0 items-center justify-between border-b border-line px-4">
        <div className="flex items-center gap-2">
          <PanelRight className="size-icon-md text-muted" />
          <span className="text-sm font-semibold text-secondary">Runtime context</span>
        </div>
        <span className="label-mono">live</span>
      </div>
      <ScrollArea className="flex-1">
        <div className="space-y-5 p-3">
          {approvals.length > 0 ? (
            <section className="space-y-2">
              <p className="label-mono text-warning">Needs approval</p>
              {approvals.map((approval) => (
                <article
                  key={approval.approval_id}
                  className="rounded-lg border border-warning/30 bg-warning/5 p-3"
                >
                  <div className="flex items-center justify-between gap-2">
                    <div className="flex items-center gap-2 text-xs font-semibold text-primary">
                      <ShieldCheck className="size-icon-md text-warning" />
                      {approval.tool.name}
                    </div>
                    <span className="label-mono">allow once</span>
                  </div>
                  <p className="mt-2 text-2xs leading-4 text-muted">
                    The runtime is waiting for a one-time decision. No permanent policy change will be
                    made.
                  </p>
                  <pre className="mt-2 max-h-28 overflow-auto whitespace-pre-wrap break-words rounded-md bg-sunken p-2 font-mono text-2xs leading-4 text-muted">
                    {JSON.stringify(approval.tool.arguments, null, 2)}
                  </pre>
                  <div className="mt-3 flex gap-2">
                    <Button
                      variant="primary"
                      size="sm"
                      block
                      onClick={() => onApprove(approval.approval_id)}
                      icon={<Check className="size-icon-sm" />}
                    >
                      Allow once
                    </Button>
                    <Button
                      variant="danger"
                      size="sm"
                      block
                      onClick={() => onDeny(approval.approval_id)}
                      icon={<X className="size-icon-sm" />}
                    >
                      Deny once
                    </Button>
                  </div>
                </article>
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

          <Section title="Tool activity" meta={`${tools.length} calls`}>
            {tools.length === 0 ? (
              <EmptyState>Tool calls will appear here.</EmptyState>
            ) : (
              <div className="space-y-1.5">
                {[...tools].reverse().map((tool) => (
                  <ToolActivityCard key={tool.id} tool={tool} />
                ))}
              </div>
            )}
          </Section>

          <Section title="Verification" meta={`${verification.length} checks`}>
            {verification.length === 0 ? (
              <EmptyState>Verification results will appear here.</EmptyState>
            ) : (
              <div className="space-y-1.5">
                {[...verification].reverse().map((item) => (
                  <VerificationCard key={item.id} item={item} />
                ))}
              </div>
            )}
          </Section>

          <Section title="Session timeline" meta="chronological">
            {timeline.length === 0 ? (
              <EmptyState>Session events will appear here.</EmptyState>
            ) : (
              <div className="space-y-0.5">
                {timeline.map((entry) => (
                  <TimelineRow key={entry.id} entry={entry} />
                ))}
              </div>
            )}
          </Section>
        </div>
      </ScrollArea>
    </aside>
  );
}

function Section({
  title,
  meta,
  children,
}: {
  title: string;
  meta: string;
  children: React.ReactNode;
}) {
  return (
    <section>
      <div className="mb-2 flex items-center justify-between">
        <p className="label-mono">{title}</p>
        <span className="text-2xs text-faint">{meta}</span>
      </div>
      {children}
    </section>
  );
}

/**
 * Collapsible row shared by tool activity and verification results.
 *
 * Both are the same shape: an icon, a label, a status, and expandable detail.
 * Keeping one implementation means the disclosure behaviour and the type scale
 * cannot drift between them.
 */
function Disclosure({
  icon,
  label,
  meta,
  tone,
  busy,
  children,
}: {
  icon: React.ReactNode;
  label: string;
  meta?: React.ReactNode;
  tone: Tone;
  busy?: boolean;
  children: React.ReactNode;
}) {
  return (
    <details className="group rounded-lg border border-line bg-elevated open:border-line-strong">
      <summary className="flex cursor-pointer list-none items-center gap-2 px-2.5 py-2 text-2xs">
        <span className={toneText(tone)}>{icon}</span>
        <span className="min-w-0 flex-1 truncate text-xs font-medium text-secondary">{label}</span>
        {meta ? <span className="shrink-0 text-2xs text-faint">{meta}</span> : null}
        <span className={cx("shrink-0 text-2xs", toneText(tone))}>{busy ? "…" : null}</span>
        <ChevronRight className="size-icon-sm shrink-0 text-faint transition-transform duration-fast group-open:rotate-90" />
      </summary>
      <div className="border-t border-line px-2.5 py-2">{children}</div>
    </details>
  );
}

function ToolActivityCard({ tool }: { tool: ToolActivity }) {
  const tone: Tone =
    tool.state === "succeeded"
      ? "success"
      : tool.state === "failed" || tool.state === "denied"
        ? "error"
        : tool.state === "running"
          ? "accent"
          : "neutral";
  const busy = tool.state === "running" || tool.state === "requested" || tool.state === "awaiting_approval";
  return (
    <Disclosure
      icon={<Wrench className="size-icon-sm" />}
      label={tool.name}
      tone={tone}
      busy={busy}
      meta={
        <>
          <span className="truncate font-mono">{tool.target || "no target"}</span>
          {tool.durationMs !== null ? (
            <span className="ml-2 inline-flex items-center gap-1">
              <Clock3 className="size-icon-xs" />
              {formatDuration(tool.durationMs)}
            </span>
          ) : null}
        </>
      }
    >
      {tool.error ? <p className="mb-2 text-2xs text-error">{tool.error}</p> : null}
      <pre className="max-h-40 overflow-auto whitespace-pre-wrap break-words font-mono text-2xs leading-4 text-muted">
        {tool.output || "No output was reported."}
      </pre>
    </Disclosure>
  );
}

function VerificationCard({ item }: { item: VerificationActivity }) {
  const tone: Tone = item.state === "passed" ? "success" : item.state === "failed" ? "error" : "accent";
  const busy = item.state === "running";
  return (
    <Disclosure
      icon={
        item.state === "passed" ? (
          <CheckCircle2 className="size-icon-sm" />
        ) : (
          <CircleDot className={cx("size-icon-sm", busy && "animate-pulse")} />
        )
      }
      label={item.category}
      tone={tone}
      busy={busy}
      meta={item.durationMs !== null ? formatDuration(item.durationMs) : null}
    >
      <p className="break-words font-mono text-2xs text-secondary">{item.command}</p>
      {item.diagnostics.length > 0 ? (
        <pre className="mt-2 max-h-32 overflow-auto whitespace-pre-wrap font-mono text-2xs leading-4 text-error">
          {item.diagnostics.join("\n")}
        </pre>
      ) : null}
    </Disclosure>
  );
}

function TimelineRow({ entry }: { entry: TimelineEntry }) {
  const tone = entry.tone === "success" ? "success" : entry.tone === "danger" ? "error" : entry.tone === "warning" ? "warning" : "accent";
  return (
    <div
      className="grid grid-cols-[14px_1fr] gap-2 rounded-md px-2 py-1.5 text-2xs leading-4 transition-colors duration-fast hover:bg-hover"
    >
      {entry.tone === "danger" ? (
        <XCircle className={cx("size-icon-sm", toneText(tone))} />
      ) : entry.tone === "success" ? (
        <CheckCircle2 className={cx("size-icon-sm", toneText(tone))} />
      ) : (
        <CircleDot className={cx("size-icon-sm", toneText(tone))} />
      )}
      <div className="min-w-0">
        <p className="truncate font-mono text-secondary">{entry.eventType}</p>
        {entry.detail ? <p className="truncate text-faint">{entry.detail}</p> : null}
      </div>
    </div>
  );
}

function formatDuration(durationMs: number): string {
  if (durationMs < 1000) return `${durationMs}ms`;
  return `${(durationMs / 1000).toFixed(1)}s`;
}

function PanelLoading({ label }: { label: string }) {
  return (
    <div className="flex min-h-0 flex-1 items-center justify-center gap-2 text-2xs text-muted">
      <LoaderCircle className="size-icon-sm animate-spin" />
      {label}
    </div>
  );
}

function StatusBar({
  status,
  runPhase,
  workspacePath,
  lastError,
  onClearError,
}: {
  status: string;
  runPhase: RunPhase;
  workspacePath: string;
  lastError: string | null;
  onClearError: () => void;
}) {
  return (
    <footer className="flex h-7 shrink-0 items-center gap-4 border-t border-line bg-app px-4 text-2xs text-faint">
      <StatusIndicator status={`runtime ${status}`} />
      <span className="flex items-center gap-1.5">
        <Terminal className="size-icon-sm" />
        {workspacePath || "no workspace"}
      </span>
      <span className="flex items-center gap-1.5">
        <Activity className="size-icon-sm" />run {runPhase}
      </span>
      <span className="ml-auto">v0 shell · runtime owns execution</span>
      {lastError ? (
        <button className="text-error transition-colors duration-fast hover:text-primary" onClick={onClearError}>
          dismiss error
        </button>
      ) : null}
    </footer>
  );
}

/**
 * Non-blocking error notice.
 *
 * Deliberately not a dialog: a runtime error should be readable and dismissible
 * without interrupting work in progress, so it sits above the status bar and
 * takes focus only when dismissed deliberately.
 */
function ErrorToast({ message, onClose }: { message: string; onClose: () => void }) {
  return (
    <div
      role="alert"
      className="fixed bottom-10 right-5 z-40 flex max-w-sm animate-slide-up items-start gap-3 rounded-lg border border-error/30 bg-overlay px-3 py-3 text-xs text-secondary shadow-overlay"
    >
      <CircleAlert className="mt-0.5 size-icon-md shrink-0 text-error" />
      <p className="flex-1 leading-5">{message}</p>
      <IconButton label="Dismiss error" size="sm" className="-mr-1 -mt-1" onClick={onClose}>
        <X className="size-icon-sm" />
      </IconButton>
    </div>
  );
}

export default App;
