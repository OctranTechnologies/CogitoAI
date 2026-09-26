import { useState } from "react";
import { Bot } from "lucide-react";
import type { ModelSettings, PermissionSettings } from "../lib/settings";
import { ComposerNotice, PromptComposer } from "./prompt-composer";

export interface LandingViewProps {
  value: string;
  onChange: (value: string) => void;
  onSubmit: (text: string) => void;
  disabled: boolean;
  running: boolean;
  onCancel: () => void;
  connected: boolean;
  workspacePath: string;
  onChooseWorkspace: () => void;
  models: ModelSettings | null;
  permissions: PermissionSettings | null;
  onSelectModel: (model: string) => void;
  onSelectMode: (mode: string) => void;
  pendingMode: string | null;
  /** Shown as a dismissible banner above the composer. */
  runtimeError: string | null;
}

/**
 * First screen when no conversation is open.
 *
 * Composition only: a glyph, one heading, an optional notice, and the composer
 * anchored low. Every control here is the same one the conversation view uses, so
 * there is no second code path to keep in step.
 */
export function LandingView({
  value,
  onChange,
  onSubmit,
  disabled,
  running,
  onCancel,
  connected,
  workspacePath,
  onChooseWorkspace,
  models,
  permissions,
  onSelectModel,
  onSelectMode,
  pendingMode,
  runtimeError,
}: LandingViewProps) {
  // A notice is a nudge, not a blocker, so it can be dismissed for the session.
  const [noticeDismissed, setNoticeDismissed] = useState(false);

  const notice = !noticeDismissed && runtimeError ? (
    <ComposerNotice
      tone="error"
      title="The runtime reported an error."
      detail={runtimeError}
      onDismiss={() => setNoticeDismissed(true)}
    />
  ) : !noticeDismissed && !connected ? (
    <ComposerNotice
      tone="warning"
      title="No runtime connected."
      detail="Start cogito-rpc-dev, then enter its address in the bar above."
    />
  ) : null;

  return (
    <section className="flex min-h-0 flex-1 flex-col">
      <div className="flex min-h-0 flex-1 items-center justify-center px-6 pb-6">
        <div className="w-full max-w-3xl">
          <div className="mb-6 flex flex-col items-center text-center">
            <span className="mb-4 flex size-11 items-center justify-center rounded-xl border border-line bg-panel text-accent">
              <Bot className="size-icon-2xl" />
            </span>
            <h1 className="text-xl font-semibold tracking-tight text-primary">
              What should we build?
            </h1>
            <p className="mt-2 max-w-md text-sm leading-6 text-muted">
              Describe a task and the agent will plan, edit, and verify it against this workspace.
            </p>
          </div>
          <PromptComposer
            value={value}
            onChange={onChange}
            onSubmit={onSubmit}
            disabled={disabled}
            running={running}
            onCancel={onCancel}
            models={models}
            permissions={permissions}
            onSelectModel={onSelectModel}
            onSelectMode={onSelectMode}
            pendingMode={pendingMode}
            workspacePath={workspacePath}
            onChooseWorkspace={onChooseWorkspace}
            size="landing"
            notice={notice}
            connected={connected}
          />
        </div>
      </div>
    </section>
  );
}
