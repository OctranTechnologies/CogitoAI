import { CheckCircle2, History, LoaderCircle, RotateCcw } from "lucide-react";
import { formatTimestamp, type CheckpointEntry } from "../lib/changes";
import type { RestoreReport } from "../lib/rpc";
import { Badge, Button, EmptyState, Tooltip } from "./ui";

interface CheckpointTimelineProps {
  checkpoints: CheckpointEntry[];
  restoringId: string | null;
  lastRestore: RestoreReport | null;
  disabled: boolean;
  onRestore: (id: string) => void;
}

/**
 * Chronological list of runtime checkpoints with a restore control for each.
 *
 * Restore always calls `checkpoint.undo` on the runtime so its existing safety
 * logic (conflict detection, scoped file list) decides what actually changes on
 * disk. The desktop never writes files itself.
 */
export function CheckpointTimeline({
  checkpoints,
  restoringId,
  lastRestore,
  disabled,
  onRestore,
}: CheckpointTimelineProps) {
  return (
    <div>
      <div className="mb-2 flex items-center justify-between">
        <p className="label-mono flex items-center gap-1.5">
          <History className="size-icon-sm" /> Checkpoints
        </p>
        <span className="text-2xs text-faint">{checkpoints.length}</span>
      </div>
      {checkpoints.length === 0 ? (
        <EmptyState>Checkpoints are recorded automatically when a run starts. Run a task to create one.</EmptyState>
      ) : (
        <ol className="space-y-2">
          {checkpoints.map((checkpoint) => {
            const restoring = restoringId === checkpoint.id;
            const restorable = checkpoint.affectedFiles.length > 0;
            return (
              <li key={checkpoint.id} className="rounded-lg border border-line bg-elevated p-2.5">
                <div className="flex items-start justify-between gap-2">
                  <code className="min-w-0 flex-1 truncate font-mono text-2xs text-secondary" title={checkpoint.id}>
                    {checkpoint.id}
                  </code>
                  {checkpoint.isRestored ? (
                    <Badge tone="success">
                      <CheckCircle2 className="size-icon-xs" /> restored
                    </Badge>
                  ) : null}
                </div>
                <p className="mt-1 text-2xs text-faint">{formatTimestamp(checkpoint.createdAt)}</p>
                <p
                  className="mt-1 line-clamp-2 text-xs leading-4 text-muted"
                  title={checkpoint.trigger || "Run started"}
                >
                  {checkpoint.trigger || "Run started"}
                </p>
                {checkpoint.affectedFiles.length > 0 ? (
                  <div className="mt-1.5 flex flex-wrap gap-1">
                    {checkpoint.affectedFiles.slice(0, 4).map((file) => (
                      <span
                        key={file}
                        className="max-w-full truncate rounded-sm bg-active px-1.5 py-0.5 font-mono text-2xs text-muted"
                        title={file}
                      >
                        {file}
                      </span>
                    ))}
                    {checkpoint.affectedFiles.length > 4 ? (
                      <span className="text-2xs text-faint">
                        +{checkpoint.affectedFiles.length - 4}
                      </span>
                    ) : null}
                  </div>
                ) : null}
                <Tooltip
                  label={
                    restorable
                      ? "Ask the runtime to revert this checkpoint's recorded changes"
                      : "This checkpoint recorded no file changes"
                  }
                >
                  <span className="mt-2 block">
                    <Button
                      size="sm"
                      block
                      disabled={disabled || restoring || !restorable}
                      onClick={() => onRestore(checkpoint.id)}
                      icon={
                        restoring ? (
                          <LoaderCircle className="size-icon-sm animate-spin" />
                        ) : (
                          <RotateCcw className="size-icon-sm" />
                        )
                      }
                    >
                      {restoring ? "Restoring…" : "Restore"}
                    </Button>
                  </span>
                </Tooltip>
              </li>
            );
          })}
        </ol>
      )}
      {lastRestore ? (
        <p className="mt-2 flex items-start gap-1.5 rounded-md border border-line bg-elevated p-2 text-2xs leading-4 text-muted">
          <CheckCircle2 className="mt-0.5 size-icon-sm shrink-0 text-success" />
          <span>
            Restored {lastRestore.restored_files.length} file
            {lastRestore.restored_files.length === 1 ? "" : "s"} through the runtime.
          </span>
        </p>
      ) : null}
    </div>
  );
}
