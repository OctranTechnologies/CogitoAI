import { memo, useState } from "react";
import {
  AlertTriangle,
  Check,
  ChevronRight,
  CircleDashed,
  FileCode2,
  GitBranch,
  ShieldAlert,
  Terminal,
} from "lucide-react";
import {
  type ActivityBlock,
  type ActivityTone,
  type ApprovalBlock,
  type AssistantBlock,
  type ErrorBlock,
  type NoticeBlock,
  type ToolBlock,
  type UserBlock,
  type VerificationBlock,
  isToolPending,
  visibleArguments,
} from "../lib/activity";
import { cx } from "./ui";

/**
 * One restrained indicator for each of the four states that matter.
 *
 * Shape carries the meaning as much as colour does, so the stream still reads
 * correctly without colour, and nothing pulses or animates except work that is
 * genuinely in progress.
 */
function Indicator({ tone }: { tone: ActivityTone }) {
  const glyph = {
    running: <CircleDashed className="size-icon-xs animate-spin" />,
    success: <Check className="size-icon-xs" />,
    failure: <AlertTriangle className="size-icon-xs" />,
    attention: <ShieldAlert className="size-icon-xs" />,
    neutral: <span className="size-icon-xs rounded-full bg-current opacity-40" />,
  }[tone];

  const colour = {
    running: "text-accent",
    success: "text-success",
    failure: "text-error",
    attention: "text-warning",
    neutral: "text-faint",
  }[tone];

  return (
    <span className={cx("flex size-icon-sm shrink-0 items-center justify-center", colour)} aria-hidden="true">
      {glyph}
    </span>
  );
}

function formatDuration(durationMs: number | null): string | null {
  if (durationMs === null) return null;
  if (durationMs < 1000) return `${durationMs}ms`;
  if (durationMs < 60000) return `${(durationMs / 1000).toFixed(1)}s`;
  return `${Math.floor(durationMs / 60000)}m ${Math.round((durationMs % 60000) / 1000)}s`;
}

/** A run's headline outcome, e.g. "14 tests passed" or "3 tests failed". */
function verificationHeadline(block: VerificationBlock): string {
  if (block.passed === null) return `Running ${block.command}`;
  const count = block.diagnostics.length;
  const noun = block.passed ? (count === 1 ? "check passed" : "checks passed") : count === 0 ? "failed" : count === 1 ? "check failed" : "checks failed";
  return `${count > 0 ? `${count} ` : ""}${noun} · ${block.command}`;
}

export function ToolRow({
  block,
  onApprove,
  onDeny,
  onOpenFile,
}: {
  block: ToolBlock;
  onApprove: (approvalId: string) => void;
  onDeny: (approvalId: string) => void;
  onOpenFile: (path: string) => void;
}) {
  const [open, setOpen] = useState(false);
  const waiting = block.phase === "awaiting_approval";
  const hasDetail =
    Object.keys(visibleArguments(block.arguments)).length > 0 || block.output !== "" || block.error !== "";
  const duration = formatDuration(block.durationMs);
  const statusText = waiting
    ? "waiting for approval"
    : block.phase === "failed" || block.phase === "denied"
      ? "failed"
      : isToolPending(block)
        ? "running"
        : "done";

  return (
    <div
      className={cx(
        "rounded-md border border-transparent px-2 py-1 transition-colors duration-fast",
        waiting && "border-warning/30 bg-warning/5",
        block.phase === "failed" && "border-error/25 bg-error/5",
      )}
      data-testid={`tool-${block.id}`}
      data-phase={block.phase}
    >
      <div className="flex items-center gap-2">
        <Indicator tone={block.tone} />
        <button
          type="button"
          onClick={() => setOpen((value) => !value)}
          disabled={!hasDetail}
          aria-expanded={open}
          className={cx(
            "flex min-w-0 flex-1 items-center gap-1.5 rounded text-left text-xs",
            hasDetail ? "cursor-pointer text-secondary hover:text-primary" : "cursor-default text-secondary",
          )}
        >
          {hasDetail ? (
            <ChevronRight className={cx("size-icon-xs shrink-0 transition-transform duration-fast", open && "rotate-90")} />
          ) : (
            <span className="size-icon-xs shrink-0" />
          )}
          <span className="truncate">{block.label}</span>
        </button>
        {duration ? <span className="shrink-0 text-2xs text-faint tabular-nums">{duration}</span> : null}
        <span className="sr-only">{statusText}</span>
      </div>

      {waiting ? (
        <div className="mt-1 space-y-1 pl-8">
          <p className="whitespace-pre-wrap break-words rounded bg-panel/70 px-2 py-1 font-mono text-2xs text-secondary">
            {JSON.stringify(visibleArguments(block.arguments), null, 2)}
          </p>
          {block.approvalRisks.length > 0 ? (
            <p className="text-2xs text-faint">Risk: {block.approvalRisks.join(" · ")}</p>
          ) : null}
          {block.approvalReason ? <p className="text-2xs text-faint">{block.approvalReason}</p> : null}
          <div className="flex items-center gap-2">
          <span className="text-2xs text-warning">Approve this action to continue</span>
          <button
            type="button"
            onClick={() => onApprove(block.approvalId!)}
            className="rounded border border-line-strong px-2 py-0.5 text-2xs text-secondary transition-colors duration-fast hover:border-success hover:text-success"
          >
            Approve
          </button>
          <button
            type="button"
            onClick={() => onDeny(block.approvalId!)}
            className="rounded border border-line-strong px-2 py-0.5 text-2xs text-secondary transition-colors duration-fast hover:border-error hover:text-error"
          >
            Deny
          </button>
          </div>
        </div>
      ) : null}

      {open && hasDetail ? (
        <div className="mt-1 pl-8">
          {Object.keys(visibleArguments(block.arguments)).length > 0 ? (
            <dl className="mb-1 flex flex-wrap gap-x-3 gap-y-0.5">
              {Object.entries(visibleArguments(block.arguments)).map(([key, value]) => (
                <div key={key} className="flex min-w-0 gap-1">
                  <dt className="text-2xs text-faint">{key}</dt>
                  <dd className="min-w-0 truncate text-2xs text-secondary">
                    {typeof value === "string" ? value : JSON.stringify(value)}
                  </dd>
                </div>
              ))}
            </dl>
          ) : null}
          {block.path ? (
            <button
              type="button"
              onClick={() => onOpenFile(block.path!)}
              className="mb-1 inline-flex items-center gap-1 text-2xs text-accent hover:underline"
            >
              <FileCode2 className="size-icon-xs" />
              Open in inspector
            </button>
          ) : null}
          {block.error ? (
            <pre className="scrollbar max-h-40 overflow-auto rounded border border-error/25 bg-error/5 p-2 text-2xs text-error">
              {block.error}
            </pre>
          ) : null}
          {block.output ? (
            <pre className="scrollbar max-h-40 overflow-auto rounded border border-line bg-panel p-2 text-2xs text-secondary">
              {block.output}
            </pre>
          ) : null}
        </div>
      ) : null}
    </div>
  );
}

function VerificationRow({ block }: { block: VerificationBlock }) {
  const [open, setOpen] = useState(false);
  const hasDetail = block.diagnostics.length > 0 || block.output !== "";
  const duration = formatDuration(block.durationMs);

  return (
    <div className="rounded-md border border-transparent px-2 py-1" data-testid={`verification-${block.id}`}>
      <div className="flex items-center gap-2">
        <Indicator tone={block.tone} />
        <button
          type="button"
          onClick={() => setOpen((value) => !value)}
          disabled={!hasDetail}
          aria-expanded={open}
          className={cx(
            "flex min-w-0 flex-1 items-center gap-1.5 rounded text-left text-xs",
            hasDetail ? "cursor-pointer text-secondary hover:text-primary" : "cursor-default text-secondary",
          )}
        >
          {hasDetail ? (
            <ChevronRight className={cx("size-icon-xs shrink-0 transition-transform duration-fast", open && "rotate-90")} />
          ) : (
            <span className="size-icon-xs shrink-0" />
          )}
          <span className="truncate">{verificationHeadline(block)}</span>
        </button>
        {duration ? <span className="shrink-0 text-2xs text-faint tabular-nums">{duration}</span> : null}
      </div>
      {open && hasDetail ? (
        <div className="mt-1 pl-8">
          {block.output ? (
            <pre className="scrollbar mb-1 max-h-40 overflow-auto rounded border border-line bg-panel p-2 text-2xs text-secondary">
              {block.output}
            </pre>
          ) : null}
          {block.diagnostics.length > 0 ? (
            <ul className="space-y-0.5">
              {block.diagnostics.map((diagnostic, index) => (
                <li key={`${block.id}:${index}`} className="text-2xs text-error">
                  {diagnostic}
                </li>
              ))}
            </ul>
          ) : null}
        </div>
      ) : null}
    </div>
  );
}

function UserRow({ block }: { block: UserBlock }) {
  return (
    <div className="px-2 py-1.5" data-testid={`user-${block.id}`}>
      <p className="label-mono mb-0.5 text-faint">You</p>
      {/* The prompt is preformatted, not a bubble: a task can be a long brief and
          wrapping it in a rounded card wastes the width it needs. */}
      <p className="whitespace-pre-wrap break-words text-xs leading-relaxed text-primary">{block.text}</p>
    </div>
  );
}

function AssistantRow({ block }: { block: AssistantBlock }) {
  const tokens =
    block.inputTokens !== null || block.outputTokens !== null
      ? `${block.inputTokens ?? 0} in · ${block.outputTokens ?? 0} out`
      : null;

  return (
    <div className="px-2 py-1.5" data-testid={`assistant-${block.id}`}>
      <p className="label-mono mb-0.5 flex items-center gap-1.5 text-faint">
        Assistant
        {block.model ? <span className="text-2xs normal-case tracking-normal">{block.model}</span> : null}
        {tokens ? <span className="text-2xs font-normal normal-case tracking-normal tabular-nums">{tokens}</span> : null}
        {block.streaming ? <span className="text-2xs normal-case tracking-normal text-accent">streaming</span> : null}
      </p>
      <div className="whitespace-pre-wrap break-words text-xs leading-relaxed text-secondary">{block.text}</div>
    </div>
  );
}

function ErrorRow({ block }: { block: ErrorBlock }) {
  return (
    <div className="rounded-md border border-error/30 bg-error/5 px-2 py-1.5" data-testid={`error-${block.id}`}>
      <p className="flex items-center gap-1.5 text-xs text-error">
        <AlertTriangle className="size-icon-xs shrink-0" />
        {block.title}
      </p>
      {block.detail ? <pre className="mt-1 whitespace-pre-wrap break-words text-2xs text-error/90">{block.detail}</pre> : null}
    </div>
  );
}

function NoticeRow({ block }: { block: NoticeBlock }) {
  return (
    <div className="flex items-center gap-2 px-2 py-1 text-2xs text-faint" data-testid={`notice-${block.id}`}>
      {block.reference?.startsWith("proc-") ? (
        <Terminal className="size-icon-xs shrink-0 opacity-60" />
      ) : (
        <GitBranch className="size-icon-xs shrink-0 opacity-60" />
      )}
      <span>{block.label}</span>
      {block.detail ? <span className="truncate opacity-80">{block.detail}</span> : null}
    </div>
  );
}

/** Shown when an approval's tool call is no longer in the retained event window. */
function ApprovalRow({
  block,
  onApprove,
  onDeny,
}: {
  block: ApprovalBlock;
  onApprove: (approvalId: string) => void;
  onDeny: (approvalId: string) => void;
}) {
  const args = visibleArguments(block.arguments);
  return (
    <div className="rounded-md border border-warning/30 bg-warning/5 px-2 py-1.5" data-testid={`approval-${block.approvalId}`}>
      <p className="mb-1 flex items-center gap-1.5 text-xs text-warning">
        <ShieldAlert className="size-icon-xs shrink-0" />
        {block.tool} needs approval
      </p>
      {Object.keys(args).length > 0 ? (
        <dl className="mb-1 flex flex-wrap gap-x-3 gap-y-0.5">
          {Object.entries(args).map(([key, value]) => (
            <div key={key} className="flex min-w-0 gap-1">
              <dt className="text-2xs text-faint">{key}</dt>
              <dd className="min-w-0 truncate text-2xs text-secondary">
                {typeof value === "string" ? value : JSON.stringify(value)}
              </dd>
            </div>
          ))}
        </dl>
      ) : null}
      {block.risks.length > 0 ? (
        <p className="mb-1 text-2xs text-faint">Risk: {block.risks.join(" · ")}</p>
      ) : null}
      {block.reason ? <p className="mb-1 text-2xs text-faint">{block.reason}</p> : null}
      <div className="flex items-center gap-2">
        <button
          type="button"
          onClick={() => onApprove(block.approvalId)}
          className="rounded border border-line-strong px-2 py-0.5 text-2xs text-secondary transition-colors duration-fast hover:border-success hover:text-success"
        >
          Approve
        </button>
        <button
          type="button"
          onClick={() => onDeny(block.approvalId)}
          className="rounded border border-line-strong px-2 py-0.5 text-2xs text-secondary transition-colors duration-fast hover:border-error hover:text-error"
        >
          Deny
        </button>
      </div>
    </div>
  );
}

export interface ActivityStreamProps {
  blocks: ActivityBlock[];
  onApprove: (approvalId: string) => void;
  onDeny: (approvalId: string) => void;
  onOpenFile: (path: string) => void;
}

function ActivityStreamImpl({ blocks, onApprove, onDeny, onOpenFile }: ActivityStreamProps) {
  return (
    <div className="space-y-0.5" data-testid="activity-stream">
      {blocks.map((block) => {
        switch (block.kind) {
          case "user":
            return <UserRow key={block.id} block={block} />;
          case "assistant":
            return <AssistantRow key={block.id} block={block} />;
          case "tool":
            return <ToolRow key={block.id} block={block} onApprove={onApprove} onDeny={onDeny} onOpenFile={onOpenFile} />;
          case "verification":
            return <VerificationRow key={block.id} block={block} />;
          case "error":
            return <ErrorRow key={block.id} block={block} />;
          case "notice":
            return <NoticeRow key={block.id} block={block} />;
          case "approval":
            return <ApprovalRow key={block.id} block={block} onApprove={onApprove} onDeny={onDeny} />;
          default:
            return null;
        }
      })}
    </div>
  );
}

export const ActivityStream = memo(ActivityStreamImpl);
