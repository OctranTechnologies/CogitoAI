import { useCallback, useEffect, useRef, useState } from "react";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { Info, LoaderCircle, Play, TerminalSquare, X } from "lucide-react";
import {
  subscribeTerminalExit,
  subscribeTerminalOutput,
  useDesktopStore,
} from "../store";
import { describeExit } from "../lib/terminal";
import { alpha, color } from "../lib/tokens";
import { desktopShortcutLabel } from "../lib/keyboard";
import { Badge, Button, IconButton, Tooltip } from "./ui";

/**
 * xterm is configured from JavaScript, so it reads the shared CSS custom
 * properties through `lib/tokens` rather than holding its own copy of the
 * palette. The background stays transparent so the panel token shows through
 * and the terminal matches the rest of the shell.
 */
const THEME = {
  background: color("surface-sunken"),
  foreground: color("text-secondary"),
  cursor: color("accent"),
  cursorAccent: color("surface-sunken"),
  selectionBackground: alpha("accent", 0.3),
  black: color("surface-sunken"),
  red: color("error"),
  green: color("success"),
  yellow: color("warning"),
  blue: color("accent"),
  magenta: color("text-muted"),
  cyan: color("accent-strong"),
  white: color("text-secondary"),
  brightBlack: color("border-strong"),
  brightRed: color("error"),
  brightGreen: color("success"),
  brightYellow: color("warning"),
  brightBlue: color("accent-strong"),
  brightMagenta: color("text-primary"),
  brightCyan: color("accent-strong"),
  brightWhite: color("text-primary"),
} as const;

const OPTIONS = {
  fontFamily: '"JetBrains Mono", ui-monospace, SFMono-Regular, monospace',
  fontSize: 12,
  lineHeight: 1.2,
  cursorBlink: true,
  cursorStyle: "bar" as const,
  convertEol: false,
  scrollback: 5000,
  theme: THEME,
};

/**
 * Interactive terminal bound to a runtime PTY.
 *
 * This is a **human** session: the person types into it directly, so the agent
 * policy engine is intentionally not involved. Agent command execution uses a
 * completely separate, policy-governed path and cannot reach this terminal.
 */
export function TerminalPanel({
  visible,
  onVisibilityChange,
}: {
  visible?: boolean;
  onVisibilityChange?: (visible: boolean) => void;
}) {
  const containerRef = useRef<HTMLDivElement | null>(null);
  const termRef = useRef<Terminal | null>(null);
  const fitRef = useRef<FitAddon | null>(null);
  const writeTerminal = useDesktopStore((state) => state.writeTerminal);
  const resizeTerminal = useDesktopStore((state) => state.resizeTerminal);
  const startTerminal = useDesktopStore((state) => state.startTerminal);
  const closeTerminal = useDesktopStore((state) => state.closeTerminal);
  const terminal = useDesktopStore((state) => state.terminal);
  const isStartingTerminal = useDesktopStore((state) => state.isStartingTerminal);
  const connected = useDesktopStore((state) => state.status === "connected");
  const workspacePath = useDesktopStore((state) => state.workspacePath);
  const [localVisibility, setLocalVisibility] = useState(false);
  const isVisible = visible ?? localVisibility;
  const setVisible = (next: boolean) => {
    onVisibilityChange?.(next);
    if (visible === undefined) setLocalVisibility(next);
  };

  // Create/dispose the xterm instance alongside the runtime session so the
  // buffer, cursor, and scrollback all reset together.
  useEffect(() => {
    if (!isVisible || !containerRef.current) return;
    const terminal = new Terminal(OPTIONS);
    const fit = new FitAddon();
    terminal.loadAddon(fit);
    terminal.open(containerRef.current);
    termRef.current = terminal;
    fitRef.current = fit;

    try {
      fit.fit();
    } catch {
      // The container can be zero-sized before layout settles; the resize
      // observer below will fit again.
    }

    const input = terminal.onData((data) => {
      void writeTerminal(data);
    });

    const stopOutput = subscribeTerminalOutput((data) => {
      terminal.write(data);
    });

    const stopExit = subscribeTerminalExit((exit) => {
      terminal.writeln("");
      terminal.write(`\u001b[90m${describeExit(exit)}\u001b[0m`);
    });

    const observer = new ResizeObserver(() => {
      try {
        fit.fit();
      } catch {
        return;
      }
      void resizeTerminal(terminal.cols, terminal.rows);
    });
    observer.observe(containerRef.current);

    return () => {
      observer.disconnect();
      input.dispose();
      stopOutput();
      stopExit();
      termRef.current = null;
      fitRef.current = null;
      terminal.dispose();
    };
  }, [isVisible, writeTerminal, resizeTerminal]);

  const focus = useCallback(() => {
    termRef.current?.focus();
  }, []);

  return (
    <section className="flex min-h-0 flex-1 flex-col border-t border-line bg-app">
      <div className="flex h-9 shrink-0 items-center justify-between gap-2 border-b border-line px-3">
        <div className="flex min-w-0 items-center gap-2">
          <TerminalSquare className="size-icon-sm shrink-0 text-muted" />
          <span className="text-xs font-medium text-secondary">Terminal</span>
          <Tooltip
            label="This shell is operated by you. Agent commands run separately through runtime policy."
          >
            <Badge tone="warning">human · not policy governed</Badge>
          </Tooltip>
        </div>
        <div className="flex shrink-0 items-center gap-1">
          <IconButton
            label={`${isVisible ? "Hide" : "Show"} terminal (${desktopShortcutLabel("toggleTerminal")})`}
            size="sm"
            aria-expanded={isVisible}
            onClick={() => setVisible(!isVisible)}
          >
            {isVisible ? <X className="size-icon-sm" /> : <TerminalSquare className="size-icon-sm" />}
          </IconButton>
          {terminal ? (
            <Button
              size="sm"
              variant="secondary"
              onClick={() => void closeTerminal()}
              title="Terminate this shell"
            >
              <X className="size-icon-xs" /> End shell
            </Button>
          ) : (
            <Button
              size="sm"
              variant="secondary"
              disabled={!connected || !workspacePath || isStartingTerminal}
              onClick={() => {
                setVisible(true);
                void startTerminal();
              }}
              title={`Open an interactive shell in the selected workspace (${desktopShortcutLabel("toggleTerminal")})`}
            >
              {isStartingTerminal ? (
                <LoaderCircle className="size-icon-xs animate-spin" />
              ) : (
                <Play className="size-icon-xs" />
              )}
              Start shell
            </Button>
          )}
        </div>
      </div>
      {isVisible && terminal ? (
        <div
          className="min-h-0 flex-1 cursor-text overflow-hidden bg-sunken px-2 py-1"
          onClick={focus}
        >
          <div ref={containerRef} className="h-full w-full" />
        </div>
      ) : (
        <TerminalUnavailable isRunning={Boolean(terminal)} isStarting={isStartingTerminal} />
      )}
    </section>
  );
}

function TerminalUnavailable({
  isRunning,
  isStarting,
}: {
  isRunning: boolean;
  isStarting: boolean;
}) {
  return (
    <p className="flex items-start gap-2 px-3 py-2 text-xs leading-4 text-faint">
      {isRunning ? (
        <Info className="mt-0.5 size-icon-sm shrink-0" />
      ) : isStarting ? (
        <LoaderCircle className="mt-0.5 size-icon-sm shrink-0 animate-spin" />
      ) : (
        <Info className="mt-0.5 size-icon-sm shrink-0" />
      )}
      {isRunning
        ? "A shell is running. Use the toggle above to show it."
        : "Start a terminal to open an interactive shell in the selected workspace. Commands you type here run as you; agent commands are policy-checked separately."}
    </p>
  );
}
