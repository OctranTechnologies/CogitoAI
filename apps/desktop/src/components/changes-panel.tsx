import { useMemo } from "react";
import { FileDiff, LoaderCircle } from "lucide-react";
import { DiffViewer, SourceViewer } from "../lib/code-viewers";
import type { ChangeEntry, FileChange, FileView } from "../lib/changes";

interface ChangesPanelProps {
  entries: ChangeEntry[];
  selectedPath: string | null;
  fileChange: FileChange | null;
  fileView: FileView | null;
  isLoading: boolean;
  isTruncated: boolean;
  totalChanged: number;
  isGitWorkspace: boolean;
  onSelect: (path: string) => void;
  onClear: () => void;
}

export function ChangesPanel({
  entries,
  selectedPath,
  fileChange,
  fileView,
  isLoading,
  isTruncated,
  totalChanged,
  isGitWorkspace,
  onSelect,
  onClear,
}: ChangesPanelProps) {
  const groups = useMemo(
    () => [
      { key: "added" as const, label: "Added", tone: "text-success", entries: entries.filter((entry) => entry.kind === "added") },
      { key: "modified" as const, label: "Modified", tone: "text-signal-400", entries: entries.filter((entry) => entry.kind === "modified") },
      { key: "deleted" as const, label: "Deleted", tone: "text-danger", entries: entries.filter((entry) => entry.kind === "deleted") },
    ],
    [entries],
  );

  return (
    <div className="flex min-h-0 flex-1">
      <div className="flex w-64 shrink-0 flex-col border-r border-ink-800 bg-ink-900/40">
        <div className="flex h-10 shrink-0 items-center justify-between border-b border-ink-800 px-3">
          <p className="mono-label">Changes</p>
          {isLoading ? <LoaderCircle size={12} className="animate-spin text-ink-500" /> : null}
        </div>
        <div className="min-h-0 flex-1 overflow-y-auto py-1">
          {entries.length === 0 ? (
            <p className="px-3 py-6 text-center text-[11px] leading-5 text-ink-600">
              {isGitWorkspace ? "No code changes in this workspace." : "Open a Git repository to see code changes."}
            </p>
          ) : (
            groups.map((group) =>
              group.entries.length === 0 ? null : (
                <section key={group.key} className="mb-1">
                  <p className={`px-3 py-1.5 text-[10px] font-medium uppercase tracking-wide ${group.tone}`}>
                    {group.label}
                    <span className="ml-1.5 text-ink-600">{group.entries.length}</span>
                  </p>
                  {group.entries.map((entry) => (
                    <button
                      key={entry.path}
                      onClick={() => onSelect(entry.path)}
                      className={`flex w-full items-center gap-1.5 px-3 py-1.5 text-left font-mono text-[11px] transition-colors ${
                        entry.path === selectedPath
                          ? "bg-signal-500/10 text-ink-100"
                          : "text-ink-400 hover:bg-ink-850 hover:text-ink-200"
                      }`}
                    >
                      <span className="min-w-0 flex-1 truncate" title={entry.path}>
                        {entry.path}
                      </span>
                      {entry.isBinary ? <span className="text-[9px] text-ink-600">bin</span> : null}
                      {entry.additions > 0 ? <span className="text-[9px] text-success">+{entry.additions}</span> : null}
                      {entry.deletions > 0 ? <span className="text-[9px] text-danger">-{entry.deletions}</span> : null}
                    </button>
                  ))}
                </section>
              ),
            )
          )}
        </div>
      </div>
      <div className="flex min-w-0 flex-1 flex-col p-3">
        {!selectedPath ? (
          <ChangesEmptyState />
        ) : fileChange ? (
          <DiffViewer change={fileChange} path={fileChange.path} />
        ) : fileView ? (
          <SourceViewer file={fileView} path={fileView.path} />
        ) : isLoading ? (
          <div className="flex flex-1 items-center justify-center gap-2 text-[11px] text-ink-500">
            <LoaderCircle size={13} className="animate-spin" /> Loading {selectedPath}…
          </div>
        ) : (
          <ChangesEmptyState onClear={onClear} />
        )}
        {isTruncated ? (
          <p className="mt-2 shrink-0 text-[10px] text-warning">
            Showing line counts for the first {entries.length} of {totalChanged} changed files. The runtime
            reports the complete list; refreshing reports more as they are inspected.
          </p>
        ) : null}
      </div>
    </div>
  );
}

function ChangesEmptyState({ onClear }: { onClear?: () => void }) {
  return (
    <div className="flex flex-1 flex-col items-center justify-center gap-2 text-center">
      <FileDiff size={20} className="text-ink-600" />
      <p className="text-xs font-medium text-ink-300">Select a file to inspect its changes</p>
      <p className="max-w-sm text-[11px] leading-4 text-ink-600">
        Diffs and source are read from the runtime and shown read-only. The desktop never edits your files.
      </p>
      {onClear ? (
        <button className="quiet-button mt-1" onClick={onClear}>
          Close viewer
        </button>
      ) : null}
    </div>
  );
}
