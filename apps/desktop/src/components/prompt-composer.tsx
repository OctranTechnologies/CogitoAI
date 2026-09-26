import { useState, type FormEvent, type ReactNode } from "react";
import { FolderGit2, LoaderCircle, Send, Square, X } from "lucide-react";
import type { ModelSettings, PermissionSettings } from "../lib/settings";
import { Button, IconButton, Tooltip, cx } from "./ui";
import { ComposerShell } from "./composer-parts";
import { useAutoGrow } from "../lib/composer";
import { AttachButton, ModelSelector, ModeSelector } from "./composer-selectors";

export interface PromptComposerProps {
  value: string;
  onChange: (value: string) => void;
  onSubmit: (text: string) => void;
  disabled: boolean;
  running: boolean;
  onCancel: () => void;
  models: ModelSettings | null;
  permissions: PermissionSettings | null;
  onSelectModel: (model: string) => void;
  onSelectMode: (mode: string) => void;
  pendingMode: string | null;
  /** Workspace path shown above the textarea, with a switch affordance. */
  workspacePath: string;
  onChooseWorkspace: () => void;
  size?: "compact" | "landing";
  /** Rendered between the workspace row and the textarea. */
  notice?: ReactNode;
  connected: boolean;
}

const LANDING_PLACEHOLDER = "Do anything in this workspace";
const COMPACT_PLACEHOLDER = "Ask the agent to inspect, change, or verify this workspace.";

/**
 * The prompt surface.
 *
 * Enter submits and Shift+Enter inserts a newline, which is what people expect
 * from a chat-style box. IME composition is respected so typing in a CJK input
 * method does not submit mid-word.
 */
export function PromptComposer({
  value,
  onChange,
  onSubmit,
  disabled,
  running,
  onCancel,
  models,
  permissions,
  onSelectModel,
  onSelectMode,
  pendingMode,
  workspacePath,
  onChooseWorkspace,
  size = "compact",
  notice,
  connected,
}: PromptComposerProps) {
  const textareaRef = useAutoGrow(value, size === "landing" ? 220 : 160, 24);
  const [focused, setFocused] = useState(false);
  const canSubmit = !disabled && !running && value.trim().length > 0;

  function submit(event?: FormEvent) {
    event?.preventDefault();
    if (!canSubmit) return;
    onSubmit(value.trim());
  }

  function onKeyDown(event: React.KeyboardEvent<HTMLTextAreaElement>) {
    if (event.key !== "Enter") return;
    // Never submit while an IME is composing: Enter is committing a candidate.
    if (event.nativeEvent.isComposing) return;
    if (event.shiftKey) return; // Shift+Enter is an explicit newline.
    event.preventDefault();
    submit();
  }

  const placeholder = !connected
    ? "Connect a runtime to send a message"
    : !workspacePath
      ? "Choose a workspace to get started"
      : size === "landing"
        ? LANDING_PLACEHOLDER
        : COMPACT_PLACEHOLDER;

  return (
    <form onSubmit={submit} className="w-full">
      {notice}
      <ComposerShell size={size}>
        {size === "landing" ? (
          <div className="mb-2 flex items-center gap-2 border-b border-line pb-2">
            <FolderGit2 className="size-icon-sm shrink-0 text-faint" />
            <span className="min-w-0 flex-1 truncate font-mono text-2xs text-muted">
              {workspacePath || "no workspace selected"}
            </span>
            <Button
              size="sm"
              variant="ghost"
              onClick={onChooseWorkspace}
              icon={<FolderGit2 className="size-icon-xs" />}
            >
              Change
            </Button>
          </div>
        ) : null}

        <textarea
          ref={textareaRef}
          aria-label="Message the agent"
          rows={size === "landing" ? 2 : 1}
          className={cx(
            "w-full resize-none bg-transparent px-1 text-sm leading-6 text-primary outline-none",
            "placeholder:text-faint scrollbar",
            size === "landing" && "min-h-16",
          )}
          placeholder={placeholder}
          value={value}
          disabled={disabled}
          onChange={(event) => onChange(event.target.value)}
          onKeyDown={onKeyDown}
          onFocus={() => setFocused(true)}
          onBlur={() => setFocused(false)}
        />

        <div className="mt-2 flex items-center gap-2">
          <AttachButton />
          <ModeSelector
            permissions={permissions}
            onSelect={onSelectMode}
            pending={pendingMode}
            disabled={!connected}
            disabledReason={!connected ? "Connect a runtime to change the execution mode" : undefined}
          />
          <ModelSelector
            models={models}
            onSelect={onSelectModel}
            disabled={!connected}
            disabledReason={!connected ? "Connect a runtime to change the model" : undefined}
          />
          <div className="ml-auto flex items-center gap-2">
            <Tooltip
              label={
                canSubmit
                  ? "Enter to send · Shift+Enter for a new line"
                  : "Type a task to send it"
              }
            >
              <span>
                {running ? (
                  <Button
                    variant="danger"
                    size="sm"
                    onClick={onCancel}
                    icon={<Square className="size-icon-xs" fill="currentColor" />}
                  >
                    Stop
                  </Button>
                ) : (
                  <Button
                    type="submit"
                    variant="primary"
                    size="sm"
                    disabled={!canSubmit}
                    icon={<Send className="size-icon-sm" />}
                  >
                    Send
                  </Button>
                )}
              </span>
            </Tooltip>
          </div>
        </div>
      </ComposerShell>
      {size === "landing" ? (
        <p className="mt-2 text-center text-2xs text-faint">
          {focused && value.trim()
            ? "Enter to send · Shift+Enter for a new line"
            : "The runtime owns execution. Nothing runs until the policy allows it."}
        </p>
      ) : null}
    </form>
  );
}

/** Dismissible, low-profile notice shown directly above the composer. */
export function ComposerNotice({
  tone,
  title,
  detail,
  action,
  onDismiss,
  onAction,
}: {
  tone: "neutral" | "warning" | "error";
  title: string;
  detail?: string;
  action?: string;
  onDismiss?: () => void;
  onAction?: () => void;
}) {
  const toneClasses =
    tone === "error"
      ? "border-error/30 bg-error/5 text-error"
      : tone === "warning"
        ? "border-warning/30 bg-warning/5 text-warning"
        : "border-line bg-panel text-muted";
  return (
    <div
      role="status"
      className={cx(
        "mb-2 flex items-start gap-2 rounded-lg border px-3 py-2 text-xs",
        toneClasses,
      )}
    >
      {tone === "neutral" ? (
        <LoaderCircle className="mt-0.5 size-icon-sm shrink-0 animate-spin" />
      ) : null}
      <span className="min-w-0 flex-1 leading-4">
        <span className="font-medium">{title}</span>
        {detail ? <span className="ml-1.5 opacity-90">{detail}</span> : null}
      </span>
      {action && onAction ? (
        <Button size="sm" variant="ghost" onClick={onAction}>
          {action}
        </Button>
      ) : null}
      {onDismiss !== undefined ? (
        <IconButton label="Dismiss notice" size="sm" onClick={onDismiss}>
          <X className="size-icon-sm" />
        </IconButton>
      ) : null}
    </div>
  );
}
