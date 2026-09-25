import { useCallback, useState } from "react";
import {
  DiffEditor,
  Editor,
  type OnMount as OnEditorMount,
  type DiffOnMount as OnDiffMount,
} from "@monaco-editor/react";
import { Columns2, Eye, FileWarning, Rows3 } from "lucide-react";
import { DIFF_OPTIONS, MONACO_THEME, READ_ONLY_OPTIONS } from "./monaco";
import type { FileChange, FileView } from "./changes";

type DiffLayout = "split" | "inline";

function useLayout(): [DiffLayout, () => void] {
  const [layout, setLayout] = useState<DiffLayout>("split");
  const toggle = useCallback(
    () => setLayout((current) => (current === "split" ? "inline" : "split")),
    [],
  );
  return [layout, toggle];
}

interface ShellProps {
  toolbar: React.ReactNode;
  children: React.ReactNode;
}

/** Shared frame for the source and diff viewers so they read as one instrument. */
function ViewerShell({ toolbar, children }: ShellProps) {
  return (
    <div className="flex min-h-0 min-w-0 flex-1 flex-col overflow-hidden rounded-lg border border-ink-800 bg-ink-900/70">
      <div className="flex h-10 shrink-0 items-center justify-between gap-2 border-b border-ink-800 bg-ink-900 px-3">
        {toolbar}
      </div>
      <div className="relative min-h-0 flex-1 bg-ink-900">{children}</div>
    </div>
  );
}

function ReadOnlyBadge() {
  return (
    <span className="mono-label flex items-center gap-1 text-ink-500" title="This viewer is read-only in v0">
      <Eye size={11} />
      read only
    </span>
  );
}

function UnsupportedNotice({ reason }: { reason: string }) {
  return (
    <div className="flex h-full flex-col items-center justify-center gap-2 px-6 text-center">
      <FileWarning size={20} className="text-ink-500" />
      <p className="text-xs font-medium text-ink-300">Preview unavailable</p>
      <p className="max-w-sm text-[11px] leading-4 text-ink-500">{reason}</p>
    </div>
  );
}

/** Read-only source viewer used for inspecting a file without editing it. */
export function SourceViewer({ file, path }: { file: FileView; path: string }) {
  const handleMount: OnEditorMount = (editor) => {
    editor.updateOptions({ readOnly: true, domReadOnly: true });
  };
  if (file.is_binary) {
    return (
      <ViewerShell
        toolbar={
          <>
            <span className="truncate font-mono text-[11px] text-ink-300">{path}</span>
            <ReadOnlyBadge />
          </>
        }
      >
        <UnsupportedNotice reason="This file is binary, so a text preview is not shown." />
      </ViewerShell>
    );
  }
  return (
    <ViewerShell
      toolbar={
        <>
          <span className="truncate font-mono text-[11px] text-ink-300">{path}</span>
          <div className="flex shrink-0 items-center gap-2">
            {file.truncated ? <span className="mono-label text-warning">truncated</span> : null}
            <span className="mono-label">{file.language}</span>
            <ReadOnlyBadge />
          </div>
        </>
      }
    >
      <Editor
        height="100%"
        theme={MONACO_THEME}
        language={file.language}
        path={`inmemory://model/${path}`}
        value={file.content}
        onMount={handleMount}
        options={READ_ONLY_OPTIONS}
        loading={<EditorLoading label="Loading viewer…" />}
      />
    </ViewerShell>
  );
}

/** Read-only per-file diff with split and inline layouts. */
export function DiffViewer({ change, path }: { change: FileChange; path: string }) {
  const [layout, toggleLayout] = useLayout();
  const handleMount: OnDiffMount = () => {
    // The diff is display-only; both sides are locked at mount.
  };
  const tone =
    change.kind === "added" ? "text-success" : change.kind === "deleted" ? "text-danger" : "text-signal-400";

  if (change.is_binary) {
    return (
      <ViewerShell
        toolbar={
          <>
            <span className="truncate font-mono text-[11px] text-ink-300">{path}</span>
            <ReadOnlyBadge />
          </>
        }
      >
        <UnsupportedNotice reason="Binary files are reported as changed but are not rendered as a text diff." />
      </ViewerShell>
    );
  }

  return (
    <ViewerShell
      toolbar={
        <>
          <div className="flex min-w-0 items-center gap-2">
            <span className="truncate font-mono text-[11px] text-ink-300">{path}</span>
            <span className={`shrink-0 text-[10px] ${tone}`}>{change.kind}</span>
            <span className="shrink-0 text-[10px] text-success">+{change.additions}</span>
            <span className="shrink-0 text-[10px] text-danger">-{change.deletions}</span>
          </div>
          <div className="flex shrink-0 items-center gap-2">
            {change.truncated ? <span className="mono-label text-warning">truncated</span> : null}
            <button
              className="icon-button h-6 w-6"
              onClick={toggleLayout}
              aria-label={layout === "split" ? "Show inline diff" : "Show split diff"}
              title={layout === "split" ? "Show inline diff" : "Show split diff"}
            >
              {layout === "split" ? <Rows3 size={13} /> : <Columns2 size={13} />}
            </button>
            <ReadOnlyBadge />
          </div>
        </>
      }
    >
      <DiffEditor
        height="100%"
        theme={MONACO_THEME}
        language={change.language}
        original={change.original}
        modified={change.modified}
        onMount={handleMount}
        options={DIFF_OPTIONS[layout]}
        loading={<EditorLoading label="Loading diff…" />}
      />
    </ViewerShell>
  );
}

function EditorLoading({ label }: { label: string }) {
  return (
    <div className="flex h-full items-center justify-center text-[11px] text-ink-500">{label}</div>
  );
}
