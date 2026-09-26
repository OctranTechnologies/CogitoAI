import { Suspense, lazy, useState } from "react";
import { PanelRight, X } from "lucide-react";
import { CheckpointTimeline } from "./checkpoint-timeline";
import type { ActivityBlock } from "../lib/activity";
import type { HarnessEvent } from "../lib/rpc";
import type { CheckpointEntry, ChangeEntry, FileChange, FileView } from "../lib/changes";
import { IconButton, ScrollArea, cx } from "./ui";

// Monaco and its language tokenizers are a large bundle, and most sessions never
// open the Changes tab. Loading it on demand keeps the shell responsive on first
// paint, and keeps it out of the bundle of tests that never render code.
const ChangesPanel = lazy(() =>
  import("./changes-panel").then((module) => ({ default: module.ChangesPanel })),
);

export type InspectorTab = "changes" | "events" | "activity" | "checkpoints";

const TABS: { id: InspectorTab; label: string }[] = [
  { id: "changes", label: "Changes" },
  { id: "events", label: "Events" },
  { id: "activity", label: "Activity" },
  { id: "checkpoints", label: "Checkpoints" },
];

/**
 * The optional right-hand inspector.
 *
 * It is closed by default so the conversation keeps the full width and the
 * screen stays quiet. Everything here is detail a person asks for, never
 * something they have to dismiss in order to work.
 */
export function Inspector({
  tab,
  onTabChange,
  onClose,
  changes,
  selectedPath,
  fileChange,
  fileView,
  isLoading,
  isTruncated,
  totalChanged,
  isGitWorkspace,
  onSelectFile,
  onClearFile,
  events,
  blocks,
  checkpoints,
  restoringId,
  lastRestore,
  restoreDisabled,
  onRestore,
}: {
  tab: InspectorTab;
  onTabChange: (tab: InspectorTab) => void;
  onClose: () => void;
  changes: ChangeEntry[];
  selectedPath: string | null;
  fileChange: FileChange | null;
  fileView: FileView | null;
  isLoading: boolean;
  isTruncated: boolean;
  totalChanged: number;
  isGitWorkspace: boolean;
  onSelectFile: (path: string) => void;
  onClearFile: () => void;
  events: HarnessEvent[];
  blocks: ActivityBlock[];
  checkpoints: CheckpointEntry[];
  restoringId: string | null;
  lastRestore: { checkpoint_id: string; restored_files: string[]; conflicts: string[] } | null;
  restoreDisabled: boolean;
  onRestore: (id: string) => void;
}) {
  return (
    <aside
      className="flex w-[420px] shrink-0 flex-col border-l border-line bg-panel"
      data-testid="inspector"
      aria-label="Inspector"
    >
      <div className="flex h-9 shrink-0 items-center justify-between border-b border-line px-2">
        <nav className="flex items-center gap-0.5" aria-label="Inspector sections">
          {TABS.map((entry) => (
            <button
              key={entry.id}
              type="button"
              onClick={() => onTabChange(entry.id)}
              aria-current={tab === entry.id ? "true" : undefined}
              className={cx(
                "rounded px-2 py-1 text-2xs transition-colors duration-fast",
                tab === entry.id
                  ? "bg-sunken text-primary"
                  : "text-faint hover:bg-sunken/60 hover:text-secondary",
              )}
            >
              {entry.label}
            </button>
          ))}
        </nav>
        <IconButton label="Close inspector" onClick={onClose}>
          <X className="size-icon-sm" />
        </IconButton>
      </div>

      <div className="min-h-0 flex-1">
        {tab === "changes" ? (
          <Suspense fallback={<p className="p-3 text-2xs text-faint">Loading code view...</p>}>
            <ChangesPanel
              entries={changes}
              selectedPath={selectedPath}
              fileChange={fileChange}
              fileView={fileView}
              isLoading={isLoading}
              isTruncated={isTruncated}
              totalChanged={totalChanged}
              isGitWorkspace={isGitWorkspace}
              onSelect={onSelectFile}
              onClear={onClearFile}
            />
          </Suspense>
        ) : null}

        {tab === "events" ? <EventLog events={events} /> : null}
        {tab === "activity" ? <ActivityDetail blocks={blocks} /> : null}
        {tab === "checkpoints" ? (
          <ScrollArea className="h-full">
            <div className="p-3">
              <CheckpointTimeline
                checkpoints={checkpoints}
                restoringId={restoringId}
                lastRestore={lastRestore}
                disabled={restoreDisabled}
                onRestore={onRestore}
              />
            </div>
          </ScrollArea>
        ) : null}
      </div>
    </aside>
  );
}

/**
 * The raw event log, verbatim.
 *
 * This exists so a surprising behaviour can be traced to the event that caused
 * it instead of guessed at, which is why the payloads are shown exactly as the
 * runtime sent them.
 */
function EventLog({ events }: { events: HarnessEvent[] }) {
  const [selected, setSelected] = useState<string | null>(null);
  const ordered = [...events].reverse();
  const active = ordered.find((event) => event.event_id === selected) ?? null;

  return (
    <div className="flex h-full min-h-0 flex-col">
      <ScrollArea className="min-h-0 flex-1">
        <ul className="p-2">
          {ordered.length === 0 ? (
            <li className="p-2 text-2xs text-faint">No events yet.</li>
          ) : (
            ordered.map((event) => (
              <li key={event.event_id}>
                <button
                  type="button"
                  onClick={() => setSelected(event.event_id === selected ? null : event.event_id)}
                  className={cx(
                    "flex w-full items-baseline gap-2 rounded px-1.5 py-1 text-left transition-colors duration-fast hover:bg-sunken/60",
                    event.event_id === selected && "bg-sunken",
                  )}
                >
                  <span className="label-mono shrink-0 text-faint">{event.event_type}</span>
                  <span className="truncate text-2xs text-faint">
                    {new Date(event.timestamp).toLocaleTimeString()}
                  </span>
                </button>
              </li>
            ))
          )}
        </ul>
      </ScrollArea>
      {active ? (
        <div className="max-h-64 shrink-0 overflow-auto border-t border-line p-2">
          <pre className="whitespace-pre-wrap break-words text-2xs text-secondary">
            {JSON.stringify(active.payload, null, 2)}
          </pre>
        </div>
      ) : null}
    </div>
  );
}

/** A per-block dump, for when the summarised stream is not enough detail. */
function ActivityDetail({ blocks }: { blocks: ActivityBlock[] }) {
  const [selected, setSelected] = useState<string | null>(null);
  const active = blocks.find((block) => block.id === selected) ?? null;

  return (
    <div className="flex h-full min-h-0 flex-col">
      <ScrollArea className="min-h-0 flex-1">
        <ul className="p-2">
          {blocks.length === 0 ? (
            <li className="p-2 text-2xs text-faint">No activity yet.</li>
          ) : (
            blocks.map((block) => (
              <li key={block.id}>
                <button
                  type="button"
                  onClick={() => setSelected(block.id === selected ? null : block.id)}
                  className={cx(
                    "flex w-full items-baseline gap-2 rounded px-1.5 py-1 text-left text-2xs transition-colors duration-fast hover:bg-sunken/60",
                    block.id === selected && "bg-sunken",
                  )}
                >
                  <span className="label-mono shrink-0 text-faint">{block.kind}</span>
                  <span className="truncate text-secondary">
                    {block.kind === "tool"
                      ? block.label
                      : block.kind === "verification"
                        ? block.command
                        : "id"}
                  </span>
                </button>
              </li>
            ))
          )}
        </ul>
      </ScrollArea>
      {active ? (
        <div className="max-h-64 shrink-0 overflow-auto border-t border-line p-2">
          <pre className="whitespace-pre-wrap break-words text-2xs text-secondary">
            {JSON.stringify(active, null, 2)}
          </pre>
        </div>
      ) : null}
    </div>
  );
}

export function InspectorToggle({ open, onClick }: { open: boolean; onClick: () => void }) {
  return (
    <IconButton label={open ? "Hide inspector" : "Show inspector"} onClick={onClick} active={open}>
      <PanelRight className="size-icon-sm" />
    </IconButton>
  );
}
