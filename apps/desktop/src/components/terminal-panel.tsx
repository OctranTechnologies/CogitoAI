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

const THEME = {
  background: "#101318",
  foreground: "#e7ebf1",
  cursor: "#38bdf8",
  cursorAccent: "#101318",
  black: "#0b0d10",
  red: "#fb7185",
  green: "#5eead4",
  yellow: "#fbbf24",
  blue: "#38bdf8",
  magenta: "#c8d0dc",
  cyan: "#7dd3fc",
  white: "#e7ebf1",
  brightBlack: "#3a4554",
  brightRed: "#fb7185",
  brightGreen: "#5eead4",
  brightYellow: "#fbbf24",
  brightBlue: "#7dd3fc",
  brightMagenta: "#e7ebf1",
  brightCyan: "#c8d0dc",
  brightWhite: "#f5f7fa",
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
export function TerminalPanel() {
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
  const [isVisible, setIsVisible] = useState(false);

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
    <section className="flex min-h-0 flex-1 flex-col border-t border-ink-800 bg-ink-950">
      <div className="flex h-9 shrink-0 items-center justify-between border-b border-ink-800 px-3">
        <div className="flex items-center gap-2">
          <TerminalSquare size={13} className="text-ink-500" />
          <span className="text-[11px] font-medium text-ink-300">Terminal</span>
          <span
            className="mono-label"
            title="This shell is operated by you. Agent commands run separately through runtime policy."
          >
            human · not policy governed
          </span>
        </div>
        <button
          className="icon-button h-6 w-6"
          onClick={() => setIsVisible((value) => !value)}
          aria-label={isVisible ? "Hide terminal" : "Show terminal"}
          aria-expanded={isVisible}
          title={isVisible ? "Hide terminal" : "Show terminal"}
        >
          {isVisible ? <X size={13} /> : <TerminalSquare size={13} />}
        </button>
        {terminal ? (
          <button
            className="quiet-button ml-1 h-6 py-1 text-[10px]"
            onClick={() => void closeTerminal()}
            title="Terminate this shell"
          >
            <X size={11} /> End shell
          </button>
        ) : (
          <button
            className="quiet-button ml-1 h-6 py-1 text-[10px]"
            disabled={!connected || !workspacePath || isStartingTerminal}
            onClick={() => {
              setIsVisible(true);
              void startTerminal();
            }}
            title="Open an interactive shell in the selected workspace"
          >
            {isStartingTerminal ? <LoaderCircle size={11} className="animate-spin" /> : <Play size={11} />}
            Start shell
          </button>
        )}
      </div>
      {isVisible && terminal ? (
        <div
          className="min-h-0 flex-1 cursor-text overflow-hidden px-2 py-1"
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

function TerminalUnavailable({ isRunning, isStarting }: { isRunning: boolean; isStarting: boolean }) {
  if (isRunning) {
    return (
      <p className="flex items-center gap-2 px-3 py-2 text-[11px] leading-4 text-ink-600">
        <Info size={12} />A shell is running. Use the toggle above to show it.
      </p>
    );
  }
  return (
    <p className="flex items-center gap-2 px-3 py-2 text-[11px] leading-4 text-ink-600">
      {isStarting ? <LoaderCircle size={12} className="animate-spin" /> : <Info size={12} />}
      Start a terminal to open an interactive shell in the selected workspace. Commands you type here run
      as you; agent commands are policy-checked separately.
    </p>
  );
}
