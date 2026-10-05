import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import {
  Activity,
  CircleAlert,
  Code2,
  ChevronDown,
  FileText,
  FileDiff,
  FolderOpen,
  LoaderCircle,
  MessageSquare,
  PanelRight,
  Play,
  PlugZap,
  RotateCcw,
  Server,
  Settings2,
  Terminal,
  ShieldCheck,
  History,
  X,
} from "lucide-react";
import {
  startEventPump,
  useDesktopStore,
  type RunPhase,
} from "./store";

import { SettingsDialog } from "./components/settings-dialog";
import { AppRail, type RailTarget } from "./components/app-rail";
import { ProjectSidebar } from "./components/project-sidebar";
import { ActivityView, HistoryView, ProjectsView } from "./components/workspace-views";
import { LandingView } from "./components/landing-view";

import { SessionWorkspace } from "./components/session-workspace";
import type { InspectorTab } from "./components/inspector";
import type { SettingsScreen } from "./lib/settings";
import { readStoredRailTarget, storeRailTarget } from "./lib/shell-prefs";
import { groupSessionsByWorkspace, sortSessionsByRecent, type SidebarProject } from "./lib/sidebar-model";
import {
  DESKTOP_COMMANDS,
  desktopShortcutLabel,
  matchesDesktopShortcut,
  paletteShortcutLabel,
  type DesktopCommandId,
} from "./lib/keyboard";
import {
  Button,
  CommandMenu,
  IconButton,
  StatusIndicator,
  Tooltip,
} from "./components/ui";



function App() {
  const [address, setAddress] = useState("auto");
  // The inspector is closed by default so the conversation keeps the full
  // width; it is opened deliberately when detail is wanted.
  const [inspectorOpen, setInspectorOpen] = useState(false);
  const [inspectorTab, setInspectorTab] = useState<InspectorTab>("changes");
  const [showSettings, setShowSettings] = useState(false);
  const [showCommands, setShowCommands] = useState(false);
  const [searchRequest, setSearchRequest] = useState(0);
  const [terminalVisible, setTerminalVisible] = useState(false);
  const [railTarget, setRailTarget] = useState<RailTarget>(readStoredRailTarget);
  const [settingsScreen, setSettingsScreen] = useState<SettingsScreen>("models");
  const [connectProviderId, setConnectProviderId] = useState<string | null>(null);
  const [pendingMode, setPendingMode] = useState<string | null>(null);
  const {
    status,
    runtimeState,
    runtimeDiagnostics,
    isRestartingRuntime,
    isRecoveringRuntime,
    clientId,
    workspacePath,
    workspace,
    sessions,
    activeSessionId,
    taskMode,
    runPhase,
    messages,
    events,
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
    isStartingTerminal,
    settings,
    modelCatalog,
    isLoadingModelCatalog,
    updateModel,
    refreshModelCatalog,
    updatePermissionMode,
    closeTerminal,
    startTerminal,
    connect,
    reconnectRuntime,
    retryRuntime,
    restartRuntime,
    openRuntimeLogs,
    loadRuntimeDiagnostics,
    setWorkspacePath,
    setComposer,
    setTaskMode,
    createSession,
    resumeSession,
    sendMessage,
    approve,
    deny,
    cancel,
    selectFile,
    clearSelectedFile,
    restoreCheckpoint,
    handleServerMessage,
    clearError,
  } = useDesktopStore();

  const autoConnectStarted = useRef(false);
  const [runtimeDiagnosticsOpen, setRuntimeDiagnosticsOpen] = useState(false);

  useEffect(() => {
    if (!clientId || status !== "connected") return;
    return startEventPump(
      clientId,
      handleServerMessage,
      () => void reconnectRuntime(),
    );
  }, [clientId, status, handleServerMessage, reconnectRuntime]);

  useEffect(() => {
    if (!workspacePath || status !== "unavailable" || autoConnectStarted.current) return;
    autoConnectStarted.current = true;
    void connect(address, workspacePath);
  }, [address, connect, status, workspacePath]);

  const connected = status === "connected";
  const running = runPhase === "pending" || runPhase === "running" || runPhase === "cancelling";
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

  async function chooseWorkspace() {
    if (running) return;
    const selected = await open({ directory: true, multiple: false, title: "Select workspace" });
    if (typeof selected !== "string" || selected === workspacePath) return;
    setWorkspacePath(selected);
    await connect(address, selected);
  }

  async function connectRuntime() {
    if (!workspacePath) {
      await chooseWorkspace();
      return;
    }
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

  // Projects come from the sessions the runtime reports, so the list reflects
  // real workspaces rather than invented entries.
  const recentSessions = useMemo(() => sortSessionsByRecent(sessions), [sessions]);
  const projects = useMemo<SidebarProject[]>(
    () => groupSessionsByWorkspace(recentSessions, workspacePath, workspace),
    [recentSessions, workspacePath, workspace],
  );


  /**
   * The composer owns submission so that Enter, Shift+Enter, and the send
   * button all funnel through the same path; the form event is only used to stop
   * the browser navigating on a native submit.
   */
  const submitPrompt = useCallback(async (text: string) => {
    await sendMessage(text);
  }, [sendMessage]);

  async function selectModel(provider: string, model: string) {
    await updateModel({
      provider,
      model,
      preference_scope: "user",
      session_id: activeSessionId ?? undefined,
      record_session_event: Boolean(activeSessionId),
    });
  }

  async function selectReasoning(effort: string) {
    await updateModel({
      reasoning_effort: effort,
      preference_scope: "user",
      session_id: activeSessionId ?? undefined,
      record_session_event: Boolean(activeSessionId),
    });
  }

  function connectProviderFromPicker(providerId: string) {
    setConnectProviderId(providerId);
    openSettings("models");
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

  function openSettings(screen: SettingsScreen = "models") {
    setSettingsScreen(screen);
    setShowSettings(true);
  }

  function showInspector(tab: InspectorTab) {
    setRailTarget("home");
    storeRailTarget("home");
    setInspectorTab(tab);
    setInspectorOpen(true);
  }

  const openTerminal = useCallback(() => {
    setRailTarget("home");
    storeRailTarget("home");
    setTerminalVisible(true);
    if (!terminal && !isStartingTerminal) void startTerminal();
  }, [isStartingTerminal, startTerminal, terminal]);

  const toggleTerminal = useCallback(() => {
    if (terminalVisible) {
      setTerminalVisible(false);
      return;
    }
    openTerminal();
  }, [openTerminal, terminalVisible]);

  // Every shortcut is defined in lib/keyboard.ts. Input-specific shortcuts are
  // handled by their owner; global actions remain available from the rest of
  // the shell without stealing keys from an open modal or palette.
  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (matchesDesktopShortcut(event, "palette") || matchesDesktopShortcut(event, "paletteAlternate")) {
        event.preventDefault();
        setShowSettings(false);
        setShowCommands((value) => !value);
        return;
      }
      if (showCommands || showSettings) return;

      if (matchesDesktopShortcut(event, "newTask")) {
        if (connected && !running && !isLoadingSession) {
          event.preventDefault();
          void createSession();
        }
      } else if (matchesDesktopShortcut(event, "searchSessions")) {
        event.preventDefault();
        setSearchRequest((value) => value + 1);
      } else if (matchesDesktopShortcut(event, "toggleTerminal")) {
        event.preventDefault();
        if (connected && workspacePath && !isStartingTerminal) toggleTerminal();
      } else if (matchesDesktopShortcut(event, "submitPrompt")) {
        const target = event.target;
        if (target instanceof HTMLElement && target.closest('[aria-label="Message the agent"]')) return;
        if (composer.trim() && connected && !running && workspacePath) {
          event.preventDefault();
          void submitPrompt(composer);
        }
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [
    composer,
    connected,
    createSession,
    isLoadingSession,
    isStartingTerminal,
    running,
    showCommands,
    showSettings,
    startTerminal,
    submitPrompt,
    terminal,
    terminalVisible,
    toggleTerminal,
    workspacePath,
  ]);

  const latestUndoableCheckpoint = [...checkpoints]
    .filter((checkpoint) => !checkpoint.isRestored && checkpoint.affectedFiles.length > 0)
    .sort((left, right) => right.createdAt - left.createdAt)[0];
  const commandIcons: Record<DesktopCommandId, ReactNode> = {
    "session.new": <Play className="size-icon-sm" />,
    "workspace.open": <FolderOpen className="size-icon-sm" />,
    "sessions.search": <MessageSquare className="size-icon-sm" />,
    "session.resume": <RotateCcw className="size-icon-sm" />,
    "model.switch": <Settings2 className="size-icon-sm" />,
    "mode.switch": <ShieldCheck className="size-icon-sm" />,
    "view.diff": <FileDiff className="size-icon-sm" />,
    "terminal.open": <Terminal className="size-icon-sm" />,
    "view.checkpoints": <History className="size-icon-sm" />,
    "change.undo": <RotateCcw className="size-icon-sm" />,
    "settings.open": <Settings2 className="size-icon-sm" />,
  };
  const commandHandlers: Record<DesktopCommandId, () => void> = {
    "session.new": () => void createSession(),
    "workspace.open": () => void chooseWorkspace(),
    "sessions.search": () => setSearchRequest((value) => value + 1),
    "session.resume": () => {
      if (activeSessionId) void resumeSession();
      else setSearchRequest((value) => value + 1);
    },
    "model.switch": () => openSettings("models"),
    "mode.switch": () => openSettings("permissions"),
    "view.diff": () => showInspector("changes"),
    "terminal.open": openTerminal,
    "view.checkpoints": () => showInspector("checkpoints"),
    "change.undo": () => {
      if (latestUndoableCheckpoint) void restoreCheckpoint(latestUndoableCheckpoint.id);
    },
    "settings.open": () => openSettings(),
  };
  const commandItems = [
    ...DESKTOP_COMMANDS.map((command) => ({
      ...command,
      icon: commandIcons[command.id],
      shortcut: "shortcut" in command ? desktopShortcutLabel(command.shortcut) : undefined,
      detail: command.id === "change.undo"
        ? latestUndoableCheckpoint?.trigger || (latestUndoableCheckpoint ? "Latest checkpoint" : undefined)
        : undefined,
      disabled:
        (command.id === "session.new" && (!connected || running || isLoadingSession)) ||
        (command.id === "workspace.open" && running) ||
        (command.id === "session.resume" && (!connected || !activeSessionId || running)) ||
        ((command.id === "model.switch" || command.id === "mode.switch") && (!connected || !settings)) ||
        ((command.id === "view.diff" || command.id === "view.checkpoints") && !connected) ||
        (command.id === "terminal.open" && (!connected || !workspacePath || isStartingTerminal)) ||
        (command.id === "change.undo" && (!connected || running || !latestUndoableCheckpoint)),
      onSelect: commandHandlers[command.id],
    })),
    ...recentSessions.map((session) => {
      const title = session.title?.trim() || (session.event_count > 1 ? "Untitled session" : "New session");
      return {
        id: `resume.${session.id}`,
        label: `Resume ${title}`,
        group: "Recent sessions",
        keywords: `${title} ${session.workspace_root} resume continue`,
        icon: <MessageSquare className="size-icon-sm" />,
        disabled: !connected || running,
        onSelect: () => void resumeSession(session.id),
      };
    }),
  ];

  const showSessionWorkspace = hasConversation || terminalVisible || inspectorOpen;

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
        sessions={recentSessions}
        activeSessionId={activeSessionId}
        workspacePath={workspacePath}
        status={status}
        projects={projects}
        searchRequest={searchRequest}
        onSelectSession={(id) => void resumeSession(id)}
        onNewSession={() => void createSession()}
        disabled={!connected || running || isLoadingSession}
      />
      <main className="flex min-w-0 flex-1 flex-col">
        <WorkspaceTopBar
          status={status}
          runtimeState={runtimeState}
          address={address}
          connected={connected}
          onAddressChange={setAddress}
          onConnect={connectRuntime}
          onChooseWorkspace={chooseWorkspace}
          chooseWorkspaceDisabled={running}
          onToggleContext={() => setInspectorOpen((value) => !value)}
          onOpenCommands={() => setShowCommands(true)}
          showContext={inspectorOpen}
        />
        <RuntimeConnectionNotice
          state={runtimeState}
          message={lastError}
          diagnostics={runtimeDiagnosticsOpen ? runtimeDiagnostics : null}
          diagnosticsOpen={runtimeDiagnosticsOpen}
          busy={isRecoveringRuntime || isRestartingRuntime}
          onRetry={() => void retryRuntime()}
          onOpenLogs={() => void openRuntimeLogs()}
          onRestart={() => void restartRuntime()}
          onToggleDiagnostics={() => {
            const opening = !runtimeDiagnosticsOpen;
            setRuntimeDiagnosticsOpen(opening);
            if (opening && !runtimeDiagnostics) void loadRuntimeDiagnostics();
          }}
        />
        {railTarget === "home" ? (
          showSessionWorkspace ? (
            <SessionWorkspace
              events={events}
              hasConversation={hasConversation}
              approvals={approvals}
              connected={connected}
              runPhase={runPhase}
              running={running}
              projectName={workspace?.repository_root ?? workspace?.current_directory ?? null}
              branch={gitStatus?.branch ?? null}
              models={settings?.models ?? null}
              modelCatalog={modelCatalog?.models ?? []}
              providerCredentials={settings?.runtime.credentials ?? []}
              isLoadingModelCatalog={isLoadingModelCatalog}
              permissions={settings?.permissions ?? null}
              composer={{
                value: composer,
                onChange: setComposer,
                onSubmit: submitPrompt,
                onCancel: cancel,
                workspacePath,
                onChooseWorkspace: chooseWorkspace,
                pendingMode,
                onSelectModel: selectModel,
                onRefreshModelCatalog: (providerId: string) => void refreshModelCatalog(providerId),
                onConnectProvider: connectProviderFromPicker,
                onSelectReasoning: selectReasoning,
                onSelectMode: selectMode,
                taskMode,
                onSelectTaskMode: setTaskMode,
              }}
              onApprove={approve}
              onDeny={deny}
              onSelectFile={(path) => void selectFile(path)}
              inspector={{
                open: inspectorOpen,
                tab: inspectorTab,
                onTabChange: setInspectorTab,
                onToggle: () => setInspectorOpen((value) => !value),
                onClose: () => setInspectorOpen(false),
              }}
              changes={{
                entries: changes.entries,
                selectedPath,
                fileChange,
                fileView,
                isLoading: isLoadingChanges || isLoadingFile,
                isTruncated: isChangesTruncated,
                totalChanged: gitStatus?.changed_files.length ?? changes.entries.length,
                isGitWorkspace: Boolean(workspace?.repository_root),
              }}
              onClearFile={clearSelectedFile}
              checkpoints={checkpoints}
              restoringId={restoringCheckpointId}
              lastRestore={lastRestore}
              restoreDisabled={!connected || running}
              onRestore={(id) => void restoreCheckpoint(id)}
              terminal={{ visible: terminalVisible, onVisibilityChange: setTerminalVisible }}
            />
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
              modelCatalog={modelCatalog?.models ?? []}
              providerCredentials={settings?.runtime.credentials ?? []}
              isLoadingModelCatalog={isLoadingModelCatalog}
              permissions={settings?.permissions ?? null}
              onSelectModel={selectModel}
              onRefreshModelCatalog={(providerId) => void refreshModelCatalog(providerId)}
              onConnectProvider={connectProviderFromPicker}
              onSelectReasoning={selectReasoning}
              onSelectMode={selectMode}
              taskMode={taskMode}
              onSelectTaskMode={setTaskMode}
              pendingMode={pendingMode}
              runtimeError={lastError}
            />
          )
        ) : railTarget === "history" ? (
          <HistoryView
            timeline={timeline}
            sessions={sessions}
            onSelectSession={(id) => void resumeSession(id)}
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
        connectProviderId={connectProviderId}
        onClose={() => setShowSettings(false)}
      />
      <CommandMenu
        open={showCommands}
        onClose={() => setShowCommands(false)}
        items={commandItems}
        label="Command palette"
        placeholder="Search actions and sessions…"
      />
      {lastError && runtimeState !== "failed" ? <ErrorToast message={lastError} onClose={clearError} /> : null}
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
  runtimeState,
  address,
  connected,
  onAddressChange,
  onConnect,
  onChooseWorkspace,
  chooseWorkspaceDisabled,
  onToggleContext,
  onOpenCommands,
  showContext,
}: {
  status: string;
  runtimeState: string;
  address: string;
  connected: boolean;
  onAddressChange: (value: string) => void;
  onConnect: () => void;
  onChooseWorkspace: () => void;
  chooseWorkspaceDisabled: boolean;
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
          placeholder="auto (workspace-specific) or 127.0.0.1:4545"
        />
        <StatusIndicator status={status} />
      </div>

      <Tooltip label="Choose workspace folder">
        <IconButton label="Choose workspace" onClick={onChooseWorkspace} disabled={chooseWorkspaceDisabled}>
          <FolderOpen className="size-icon-lg" />
        </IconButton>
      </Tooltip>

      <Button
        variant="primary"
        size="sm"
        onClick={onConnect}
        disabled={connected || status === "connecting"}
        icon={
          status === "connecting" || runtimeState === "starting" || runtimeState === "reconnecting" ? (
            <LoaderCircle className="size-icon-sm animate-spin" />
          ) : (
            <PlugZap className="size-icon-sm" />
          )
        }
      >
        {connected ? "Connected" : status === "connecting" ? "Connecting…" : "Connect"}
      </Button>

      <Tooltip label={`Command palette (${paletteShortcutLabel()})`}>
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







/**
 * Collapsible row shared by tool activity and verification results.
 *
 * Both are the same shape: an icon, a label, a status, and expandable detail.
 * Keeping one implementation means the disclosure behaviour and the type scale
 * cannot drift between them.
 */






export function RuntimeConnectionNotice({
  state,
  message,
  diagnostics,
  diagnosticsOpen,
  busy,
  onRetry,
  onOpenLogs,
  onRestart,
  onToggleDiagnostics,
}: {
  state: string;
  message: string | null;
  diagnostics: { endpoint: string | null; workspace_root: string | null; retry_count: number; last_error: string | null } | null;
  diagnosticsOpen: boolean;
  busy: boolean;
  onRetry: () => void;
  onOpenLogs: () => void;
  onRestart: () => void;
  onToggleDiagnostics: () => void;
}) {
  if (state === "connected" || state === "disconnected") return null;

  if (state !== "failed") {
    const label =
      state === "starting"
        ? "Starting harness…"
        : state === "reconnecting"
          ? "Reconnecting…"
          : state === "discovering"
            ? "Connecting to harness…"
            : "Connecting…";
    return (
      <div className="flex h-9 shrink-0 items-center gap-2 border-b border-line bg-panel/60 px-4 text-xs text-muted" role="status" aria-live="polite">
        <LoaderCircle className="size-icon-sm animate-spin text-accent" />
        <span>{label}</span>
      </div>
    );
  }

  return (
    <section
      className="shrink-0 border-b border-line bg-panel/70 px-4 py-3"
      role="alert"
      aria-live="assertive"
      aria-label="Harness runtime recovery"
    >
      <div className="flex flex-wrap items-center gap-x-3 gap-y-2">
        <CircleAlert className="size-icon-md shrink-0 text-warning" />
        <div className="min-w-48 flex-1">
          <p className="text-sm font-medium text-primary">Harness isn’t available</p>
          <p className="mt-0.5 text-xs leading-5 text-muted">
            {message ?? "The local runtime stopped responding."} Your workspace and session are preserved.
          </p>
        </div>
        <div className="flex flex-wrap items-center gap-1.5">
          <Button variant="primary" size="sm" onClick={onRetry} disabled={busy} icon={<RotateCcw className="size-icon-sm" />}>
            Retry
          </Button>
          <Button variant="secondary" size="sm" onClick={onRestart} disabled={busy} icon={<RotateCcw className="size-icon-sm" />}>
            Restart runtime
          </Button>
          <Button variant="ghost" size="sm" onClick={onOpenLogs} disabled={busy} icon={<FileText className="size-icon-sm" />}>
            Open logs
          </Button>
          <Button
            variant="ghost"
            size="sm"
            onClick={onToggleDiagnostics}
            aria-expanded={diagnosticsOpen}
            icon={<ChevronDown className={`size-icon-sm transition-transform duration-fast ${diagnosticsOpen ? "rotate-180" : ""}`} />}
          >
            Details
          </Button>
        </div>
      </div>
      {diagnosticsOpen ? (
        <dl className="mt-3 grid gap-x-4 gap-y-1 border-t border-line pt-2 text-2xs sm:grid-cols-[max-content_minmax(0,1fr)]">
          <dt className="text-faint">Endpoint</dt>
          <dd className="break-all font-mono text-muted">{diagnostics?.endpoint ?? "Loading…"}</dd>
          <dt className="text-faint">Workspace</dt>
          <dd className="break-all font-mono text-muted">{diagnostics?.workspace_root ?? "Loading…"}</dd>
          <dt className="text-faint">Readiness retries</dt>
          <dd className="font-mono text-muted">{diagnostics?.retry_count ?? "—"}</dd>
          <dt className="text-faint">Last error</dt>
          <dd className="break-words font-mono text-muted">{diagnostics?.last_error ?? "Loading…"}</dd>
        </dl>
      ) : null}
    </section>
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
