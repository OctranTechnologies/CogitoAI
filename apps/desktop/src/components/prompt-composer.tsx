import { useState, type FormEvent, type ReactNode } from "react";
import { FolderGit2, LoaderCircle, Paperclip, Send, Square, X } from "lucide-react";
import type { ModelDescriptor, ModelSettings, PermissionSettings, ProviderCredentialStatus } from "../lib/settings";
import type { InputAttachment, TaskMode } from "../lib/rpc";
import { readInputAttachments, validateAttachmentCollection } from "../lib/attachments";
import { Button, IconButton, Tooltip, cx } from "./ui";
import { ComposerShell } from "./composer-parts";
import { useAutoGrow } from "../lib/composer";
import { AttachButton, ModelSelector, ModeSelector, TaskModeSelector } from "./composer-selectors";
import { desktopShortcutLabel, matchesDesktopShortcut } from "../lib/keyboard";

export interface PromptComposerProps {
  value: string;
  onChange: (value: string) => void;
  onSubmit: (text: string, attachments?: InputAttachment[]) => void | Promise<void>;
  disabled: boolean;
  running: boolean;
  onCancel: () => void;
  models: ModelSettings | null;
  modelCatalog: ModelDescriptor[];
  providerCredentials: ProviderCredentialStatus[];
  isLoadingModelCatalog: boolean;
  permissions: PermissionSettings | null;
  onSelectModel: (provider: string, model: string) => void;
  onRefreshModelCatalog: (providerId: string) => void;
  onConnectProvider: (providerId: string) => void;
  onSelectReasoning: (effort: string) => void;
  onSelectMode: (mode: string) => void;
  taskMode: TaskMode;
  onSelectTaskMode: (mode: TaskMode) => void;
  pendingMode: string | null;
  /** Workspace path shown above the textarea, with a switch affordance. */
  workspacePath: string;
  onChooseWorkspace: () => void;
  size?: "compact" | "landing";
  /** Rendered between the workspace row and the textarea. */
  notice?: ReactNode;
  connected: boolean;
  attachments?: InputAttachment[];
  onAttachmentsChange?: (attachments: InputAttachment[]) => void;
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
  modelCatalog,
  providerCredentials,
  isLoadingModelCatalog,
  permissions,
  onSelectModel,
  onRefreshModelCatalog,
  onConnectProvider,
  onSelectReasoning,
  onSelectMode,
  taskMode,
  onSelectTaskMode,
  pendingMode,
  workspacePath,
  onChooseWorkspace,
  size = "compact",
  notice,
  connected,
  attachments = [],
  onAttachmentsChange,
}: PromptComposerProps) {
  const textareaRef = useAutoGrow(value, size === "landing" ? 220 : 160, 24);
  const [focused, setFocused] = useState(false);
  const [attachmentError, setAttachmentError] = useState<string | null>(null);
  const [draggingFiles, setDraggingFiles] = useState(false);
  const canSubmit = !disabled && !running && (value.trim().length > 0 || attachments.length > 0);

  async function addFiles(files: File[]) {
    if (files.length === 0) return;
    try {
      const added = await readInputAttachments(files);
      const combined = [...attachments, ...added];
      validateAttachmentCollection(combined);
      onAttachmentsChange?.(combined);
      setAttachmentError(null);
    } catch (error) {
      setAttachmentError(error instanceof Error ? error.message : "Could not read the selected files.");
    }
  }

  function submit(event?: FormEvent) {
    event?.preventDefault();
    if (!canSubmit) return;
    void onSubmit(value.trim(), attachments);
  }

  function onKeyDown(event: React.KeyboardEvent<HTMLTextAreaElement>) {
    const modifiedSubmit = matchesDesktopShortcut(event, "submitPrompt");
    if (event.key !== "Enter") return;
    // Never submit while an IME is composing: Enter is committing a candidate.
    if (event.nativeEvent.isComposing) return;
    if (event.shiftKey) return; // Shift+Enter is an explicit newline.
    event.preventDefault();
    if (modifiedSubmit) event.stopPropagation();
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
    <form
      onSubmit={submit}
      className="relative w-full"
      onDragOver={(event) => {
        if (!event.dataTransfer.types.includes("Files")) return;
        event.preventDefault();
        if (!disabled && !running) setDraggingFiles(true);
      }}
      onDragLeave={(event) => {
        if (!event.currentTarget.contains(event.relatedTarget as Node | null)) setDraggingFiles(false);
      }}
      onDrop={(event) => {
        if (!event.dataTransfer.files.length) return;
        event.preventDefault();
        setDraggingFiles(false);
        if (!disabled && !running) void addFiles(Array.from(event.dataTransfer.files));
      }}
    >
      {notice}
      {draggingFiles ? (
        <div className="pointer-events-none absolute inset-0 z-10 flex items-center justify-center rounded-xl border border-accent/60 bg-app/90 text-sm text-primary">
          Drop screenshots or text files to attach
        </div>
      ) : null}
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

        {attachments.length ? (
          <div className="mb-2 flex flex-wrap gap-1.5" aria-label="Attached files">
            {attachments.map((attachment, index) => (
              <span
                key={`${attachment.file_name}-${index}`}
                className="inline-flex max-w-full items-center gap-1.5 rounded-md border border-line bg-panel px-2 py-1 text-2xs text-secondary"
                title={attachment.file_name}
              >
                <Paperclip className="size-icon-xs shrink-0 text-faint" />
                <span className="max-w-48 truncate">{attachment.file_name}</span>
                <button
                  type="button"
                  aria-label={`Remove ${attachment.file_name}`}
                  className="rounded text-faint hover:text-primary"
                  onClick={() => onAttachmentsChange?.(attachments.filter((_, itemIndex) => itemIndex !== index))}
                >
                  <X className="size-3" />
                </button>
              </span>
            ))}
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

        <div className="mt-2 flex flex-wrap items-center gap-2">
          <AttachButton disabled={disabled || running} onFilesSelected={(files) => void addFiles(files)} />
          <ModeSelector
            permissions={permissions}
            onSelect={onSelectMode}
            pending={pendingMode}
            disabled={!connected}
            disabledReason={!connected ? "Connect a runtime to change the execution mode" : undefined}
          />
          <TaskModeSelector
            mode={taskMode}
            onSelect={onSelectTaskMode}
            disabled={!connected || running}
            disabledReason={
              !connected
                ? "Connect a runtime to change task behavior"
                : running
                  ? "Task behavior cannot change while a task is running"
                  : undefined
            }
          />
          <ModelSelector
            models={models}
            catalog={modelCatalog}
            credentials={providerCredentials}
            loading={isLoadingModelCatalog}
            onSelect={onSelectModel}
            onRefresh={onRefreshModelCatalog}
            onConnect={onConnectProvider}
            onSelectReasoning={onSelectReasoning}
            disabled={!connected}
            disabledReason={!connected ? "Connect a runtime to change the model" : undefined}
          />
          <div className="ml-auto flex items-center gap-2">
            <Tooltip
              label={
                canSubmit
                  ? `Enter to send · Shift+Enter for a new line · ${desktopShortcutLabel("submitPrompt")} submits from anywhere`
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
        {attachmentError ? <p role="alert" className="mt-2 text-xs text-error">{attachmentError}</p> : null}
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
