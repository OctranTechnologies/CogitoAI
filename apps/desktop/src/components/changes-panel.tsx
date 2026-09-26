import { useMemo } from "react";
import { FileDiff, LoaderCircle } from "lucide-react";
import { DiffViewer, SourceViewer } from "../lib/code-viewers";
import type { ChangeEntry, FileChange, FileView } from "../lib/changes";
import { Button, toneText, type Tone } from "./ui";
import { cx } from "./ui/cx";

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
    () =>
      (
        [
          { key: "added", label: "Added", tone: "success" },
          { key: "modified", label: "Modified", tone: "accent" },
          { key: "deleted", label: "Deleted", tone: "error" },
        ] as const
      )
        .map((group) => ({
          ...group,
          entries: entries.filter((entry) => entry.kind === group.key),
        })),
    [entries],
  );

  return (
    <div className="flex min-h-0 flex-1">
      <div className="flex w-64 shrink-0 flex-col border-r border-line bg-panel">
        <div className="flex h-10 shrink-0 items-center justify-between border-b border-line px-3">
          <p className="label-mono">Changes</p>
          {isLoading ? <LoaderCircle className="size-icon-sm animate-spin text-muted" /> : null}
        </div>
        <div className="scroll-area flex-1 py-1">
          {entries.length === 0 ? (
            <p className="px-3 py-6 text-center text-2xs leading-5 text-faint">
              {isGitWorkspace
                ? "No code changes in this workspace."
                : "Open a Git repository to see code changes."}
            </p>
          ) : (
            groups.map((group) =>
              group.entries.length === 0 ? null : (
                <section key={group.key} className="mb-1">
                  <p
                    className={cx(
                      "flex items-center gap-1.5 px-3 py-1.5 text-2xs font-medium uppercase tracking-wide",
                      toneText(group.tone as Tone),
                    )}
                  >
                    {group.label}
                    <span className="text-faint">{group.entries.length}</span>
                  </p>
                  {group.entries.map((entry) => (
                    <button
                      key={entry.path}
                      onClick={() => onSelect(entry.path)}
                      className={cx(
                        "flex w-full items-center gap-1.5 px-3 py-1.5 text-left font-mono text-xs",
                        "transition-colors duration-fast",
                        entry.path === selectedPath
                          ? "bg-accent/10 text-primary"
                          : "text-muted hover:bg-hover hover:text-primary",
                      )}
                    >
                      <span className="min-w-0 flex-1 truncate" title={entry.path}>
                        {entry.path}
                      </span>
                      {entry.isBinary ? <span className="text-2xs text-faint">bin</span> : null}
                      {entry.additions > 0 ? (
                        <span className="text-2xs text-success">+{entry.additions}</span>
                      ) : null}
                      {entry.deletions > 0 ? (
                        <span className="text-2xs text-error">-{entry.deletions}</span>
                      ) : null}
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
          <div className="flex flex-1 items-center justify-center gap-2 text-2xs text-muted">
            <LoaderCircle className="size-icon-sm animate-spin" /> Loading {selectedPath}…
          </div>
        ) : (
          <ChangesEmptyState onClear={onClear} />
        )}
        {isTruncated ? (
          <p className="mt-2 shrink-0 text-2xs text-warning">
            Showing line counts for the first {entries.length} of {totalChanged} changed files. The
            runtime reports the complete list; refreshing reports more as they are inspected.
          </p>
        ) : null}
      </div>
    </div>
  );
}

function ChangesEmptyState({ onClear }: { onClear?: () => void }) {
  return (
    <div className="flex flex-1 flex-col items-center justify-center gap-2 text-center">
      <FileDiff className="size-icon-xl text-faint" />
      <p className="text-xs font-medium text-secondary">Select a file to inspect its changes</p>
      <p className="max-w-sm text-2xs leading-4 text-faint">
        Diffs and source are read from the runtime and shown read-only. The desktop never edits your
        files.
      </p>
      {onClear ? (
        <Button size="sm" className="mt-1" onClick={onClear}>
          Close viewer
        </Button>
      ) : null}
    </div>
  );
}
