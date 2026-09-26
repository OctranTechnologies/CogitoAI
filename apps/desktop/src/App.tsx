import { useEffect, useMemo, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import {
  Activity,
  CircleAlert,
  Code2,
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
  Square,
  Terminal,
  X,
} from "lucide-react";
import {
  startEventPump,
  useDesktopStore,
  type RunPhase,
} from "./store";

import { SettingsDialog } from "./components/settings-dialog";
import { AppRail, type RailTarget } from "./components/app-rail";
import { ProjectSidebar, type ProjectEntry } from "./components/project-sidebar";
import { ActivityView, HistoryView, ProjectsView } from "./components/workspace-views";
import { LandingView } from "./components/landing-view";

import { SessionWorkspace } from "./components/session-workspace";
import type { InspectorTab } from "./components/inspector";
import type { SettingsScreen } from "./lib/settings";
import { readStoredRailTarget, storeRailTarget } from "./lib/shell-prefs";
import {
  Button,
  CommandMenu,
  IconButton,
  StatusIndicator,
  Tooltip,
} from "./components/ui";



function App() {
  const [address, setAddress] = useState("127.0.0.1:4545");
  // The inspector is closed by default so the conversation keeps the full
  // width; it is opened deliberately when detail is wanted.
  const [inspectorOpen, setInspectorOpen] = useState(false);
  const [inspectorTab, setInspectorTab] = useState<InspectorTab>("changes");
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
      onSelect: () => {
        setInspectorTab("changes");
        setInspectorOpen(true);
      },
    },
    {
      id: "view.events",
      label: "Show event details",
      icon: <MessageSquare className="size-icon-sm" />,
      onSelect: () => {
        setInspectorTab("events");
        setInspectorOpen(true);
      },
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
      label: inspectorOpen ? "Hide inspector" : "Show inspector",
      icon: <PanelRight className="size-icon-sm" />,
      onSelect: () => setInspectorOpen((value) => !value),
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
          onToggleContext={() => setInspectorOpen((value) => !value)}
          onOpenCommands={() => setShowCommands(true)}
          showContext={inspectorOpen}
        />
        {railTarget === "home" ? (
          hasConversation ? (
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
                onSelectMode: selectMode,
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
              permissions={settings?.permissions ?? null}
              onSelectModel={selectModel}
              onSelectMode={selectMode}
              pendingMode={pendingMode}
              runtimeError={lastError}
            />
          )
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







/**
 * Collapsible row shared by tool activity and verification results.
 *
 * Both are the same shape: an icon, a label, a status, and expandable detail.
 * Keeping one implementation means the disclosure behaviour and the type scale
 * cannot drift between them.
 */






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
