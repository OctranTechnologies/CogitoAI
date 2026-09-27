import { ArrowDown } from "lucide-react";
import { ActivityStream } from "./activity-stream";
import { Inspector, InspectorToggle, type InspectorTab } from "./inspector";
import { PromptComposer } from "./prompt-composer";
import { SessionHeader } from "./session-header";
import { TerminalPanel } from "./terminal-panel";
import { buildActivityStream, summariseStream, type ActivityBlock } from "../lib/activity";
import { useAutoScroll } from "../lib/auto-scroll";
import type { ModelSettings, PermissionSettings } from "../lib/settings";
import type { HarnessEvent } from "../lib/rpc";
import type { CheckpointEntry, ChangeEntry, FileChange, FileView } from "../lib/changes";
import type { RunPhase } from "../lib/events";

export interface SessionWorkspaceProps {
  /** The runtime event log, which is the only source of what happened. */
  events: HarnessEvent[];
  hasConversation: boolean;
  approvals: { approval_id: string; tool: { name: string; arguments: Record<string, unknown> } }[];

  connected: boolean;
  runPhase: RunPhase;
  running: boolean;

  projectName: string | null;
  branch: string | null;
  models: ModelSettings | null;
  permissions: PermissionSettings | null;

  composer: {
    value: string;
    onChange: (value: string) => void;
    onSubmit: (value: string) => void;
    onCancel: () => void;
    workspacePath: string;
    onChooseWorkspace: () => void;
    pendingMode: string | null;
    onSelectModel: (model: string) => void;
    onSelectMode: (mode: string) => void;
  };
  terminal?: {
    visible: boolean;
    onVisibilityChange: (visible: boolean) => void;
  };

  onApprove: (approvalId: string) => void;
  onDeny: (approvalId: string) => void;
  onSelectFile: (path: string) => void;

  inspector: {
    open: boolean;
    tab: InspectorTab;
    onTabChange: (tab: InspectorTab) => void;
    onToggle: () => void;
    onClose: () => void;
  };

  changes: {
    entries: ChangeEntry[];
    selectedPath: string | null;
    fileChange: FileChange | null;
    fileView: FileView | null;
    isLoading: boolean;
    isTruncated: boolean;
    totalChanged: number;
    isGitWorkspace: boolean;
  };
  onClearFile: () => void;
  checkpoints: CheckpointEntry[];
  restoringId: string | null;
  lastRestore: { checkpoint_id: string; restored_files: string[]; conflicts: string[] } | null;
  restoreDisabled: boolean;
  onRestore: (id: string) => void;
}

/**
 * The active session's main area: a quiet header, one scrolling activity stream,
 * an optional inspector, and the composer pinned to the bottom.
 *
 * The stream is derived from the event log here rather than threaded in, so
 * there is exactly one place that decides what a session looks like.
 */
export function SessionWorkspace(props: SessionWorkspaceProps) {
  const blocks: ActivityBlock[] = buildActivityStream(props.events, { approvals: props.approvals });
  const summary = summariseStream(blocks);

  // Re-derive and follow whenever the log grows.
  const { ref, following, resume } = useAutoScroll(blocks.length);

  const activeTools = blocks.filter(
    (block) => block.kind === "tool" && (block.phase === "requested" || block.phase === "running"),
  ).length;

  return (
    <section className="flex min-h-0 min-w-0 flex-1 flex-col" data-testid="session-workspace">
      <div className="flex min-w-0 shrink-0 items-center">
        <SessionHeader
          projectName={props.projectName}
          branch={props.branch}
          connected={props.connected}
          runPhase={props.runPhase}
          models={props.models}
          permissions={props.permissions}
          activeTools={activeTools}
          failures={summary.failures}
        />
        <div className="ml-auto shrink-0 px-2">
          <InspectorToggle open={props.inspector.open} onClick={props.inspector.onToggle} />
        </div>
      </div>

      <div className="relative flex min-h-0 flex-1">
        <div className="flex min-w-0 flex-1 flex-col">
          <div className="relative min-h-0 flex-1">
            <div
              ref={ref}
              className="scrollbar h-full overflow-y-auto overscroll-contain px-3 py-2"
              data-testid="activity-scroll"
            >
              {props.hasConversation || blocks.length > 0 ? (
                <div className="mx-auto max-w-3xl">
                  <ActivityStream
                    blocks={blocks}
                    onApprove={props.onApprove}
                    onDeny={props.onDeny}
                    onOpenFile={props.onSelectFile}
                  />
                </div>
              ) : (
                <p className="mx-auto max-w-3xl px-2 py-8 text-center text-xs text-faint">
                  Send a task to start a durable session. Tool activity, approvals, and verification appear
                  here as the runtime reports them.
                </p>
              )}
            </div>

            {/* Offered only once the reader has scrolled away, so following the
                tail is always a choice they can see and take back. */}
            {!following ? (
              <button
                type="button"
                onClick={resume}
                className="absolute bottom-3 left-1/2 flex -translate-x-1/2 items-center gap-1 rounded-full border border-line-strong bg-panel px-2.5 py-1 text-2xs text-secondary shadow-panel transition-colors duration-fast hover:text-primary"
              >
                <ArrowDown className="size-icon-xs" />
                Jump to latest
              </button>
            ) : null}
          </div>

          {/* The composer is pinned to the bottom so the task is always in the
              same place no matter how much activity has scrolled past. The human
              terminal sits directly above it and renders nothing when closed. */}
          <TerminalPanel
            visible={props.terminal?.visible}
            onVisibilityChange={props.terminal?.onVisibilityChange}
          />
          <div className="shrink-0 border-t border-line bg-app px-4 py-3">
            <div className="mx-auto max-w-3xl">
              <PromptComposer
                value={props.composer.value}
                onChange={props.composer.onChange}
                onSubmit={props.composer.onSubmit}
                disabled={!props.connected || props.running || !props.composer.workspacePath}
                running={props.running}
                onCancel={props.composer.onCancel}
                models={props.models}
                permissions={props.permissions}
                onSelectModel={props.composer.onSelectModel}
                onSelectMode={props.composer.onSelectMode}
                pendingMode={props.composer.pendingMode}
                workspacePath={props.composer.workspacePath}
                onChooseWorkspace={props.composer.onChooseWorkspace}
                connected={props.connected}
              />
            </div>
          </div>
        </div>

        {props.inspector.open ? (
          <Inspector
            tab={props.inspector.tab}
            onTabChange={props.inspector.onTabChange}
            onClose={props.inspector.onClose}
            changes={props.changes.entries}
            selectedPath={props.changes.selectedPath}
            fileChange={props.changes.fileChange}
            fileView={props.changes.fileView}
            isLoading={props.changes.isLoading}
            isTruncated={props.changes.isTruncated}
            totalChanged={props.changes.totalChanged}
            isGitWorkspace={props.changes.isGitWorkspace}
            onSelectFile={props.onSelectFile}
            onClearFile={props.onClearFile}
            events={props.events}
            blocks={blocks}
            checkpoints={props.checkpoints}
            restoringId={props.restoringId}
            lastRestore={props.lastRestore}
            restoreDisabled={props.restoreDisabled}
            onRestore={props.onRestore}
          />
        ) : null}
      </div>
    </section>
  );
}
