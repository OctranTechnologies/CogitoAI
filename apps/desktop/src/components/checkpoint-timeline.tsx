import {
  CheckCircle2,
  History,
  LoaderCircle,
  RotateCcw,
} from "lucide-react";
import { formatTimestamp, type CheckpointEntry } from "../lib/changes";
import type { RestoreReport } from "../lib/rpc";

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
        <p className="mono-label flex items-center gap-1.5">
          <History size={11} /> Checkpoints
        </p>
        <span className="text-[10px] text-ink-600">{checkpoints.length}</span>
      </div>
      {checkpoints.length === 0 ? (
        <p className="rounded-md border border-dashed border-ink-800 p-3 text-xs leading-5 text-ink-600">
          Checkpoints are recorded automatically when a run starts. Run a task to create one.
        </p>
      ) : (
        <ol className="space-y-2">
          {checkpoints.map((checkpoint) => {
            const restoring = restoringId === checkpoint.id;
            const restorable = checkpoint.affectedFiles.length > 0;
            return (
              <li key={checkpoint.id} className="rounded-lg border border-ink-800 bg-ink-900/70 p-2.5">
                <div className="flex items-start justify-between gap-2">
                  <code
                    className="min-w-0 flex-1 truncate font-mono text-[10px] text-ink-300"
                    title={checkpoint.id}
                  >
                    {checkpoint.id}
                  </code>
                  {checkpoint.isRestored ? (
                    <span className="flex shrink-0 items-center gap-1 text-[9px] text-success">
                      <CheckCircle2 size={10} /> restored
                    </span>
                  ) : null}
                </div>
                <p className="mt-1 text-[10px] text-ink-600">{formatTimestamp(checkpoint.createdAt)}</p>
                <p
                  className="mt-1 line-clamp-2 text-[11px] leading-4 text-ink-400"
                  title={checkpoint.trigger || "Run started"}
                >
                  {checkpoint.trigger || "Run started"}
                </p>
                {checkpoint.affectedFiles.length > 0 ? (
                  <div className="mt-1.5 flex flex-wrap gap-1">
                    {checkpoint.affectedFiles.slice(0, 4).map((file) => (
                      <span
                        key={file}
                        className="max-w-full truncate rounded bg-ink-850 px-1.5 py-0.5 font-mono text-[9px] text-ink-500"
                        title={file}
                      >
                        {file}
                      </span>
                    ))}
                    {checkpoint.affectedFiles.length > 4 ? (
                      <span className="text-[9px] text-ink-600">+{checkpoint.affectedFiles.length - 4}</span>
                    ) : null}
                  </div>
                ) : null}
                <button
                  className="quiet-button mt-2 w-full justify-center py-1.5 text-[10px]"
                  disabled={disabled || restoring || !restorable}
                  title={
                    restorable
                      ? "Ask the runtime to revert this checkpoint's recorded changes"
                      : "This checkpoint recorded no file changes"
                  }
                  onClick={() => onRestore(checkpoint.id)}
                >
                  {restoring ? (
                    <LoaderCircle size={12} className="animate-spin" />
                  ) : (
                    <RotateCcw size={12} />
                  )}
                  {restoring ? "Restoring…" : "Restore"}
                </button>
              </li>
            );
          })}
        </ol>
      )}
      {lastRestore ? (
        <div className="mt-2 flex items-start gap-1.5 rounded-md border border-ink-800 bg-ink-900/60 p-2 text-[10px] leading-4 text-ink-400">
          <CheckCircle2 size={12} className="mt-0.5 shrink-0 text-success" />
          <span>
            Restored {lastRestore.restored_files.length} file
            {lastRestore.restored_files.length === 1 ? "" : "s"} through the runtime.
          </span>
        </div>
      ) : null}
    </div>
  );
}
