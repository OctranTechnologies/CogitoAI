# CogitoAI desktop shell

This directory contains the Tauri 2 + React + TypeScript desktop client. It is a
presentation layer over the versioned `harness-rpc` loopback protocol. Tauri
commands only manage the RPC transport; agent logic remains in the Rust runtime.

For the project as a whole, see the [root README](../../README.md). For crate
boundaries, see [`docs/architecture.md`](../../docs/architecture.md).

## Prerequisites

- Rust 1.78+
- Node.js 20+
- pnpm 10+
- Tauri 2 platform prerequisites: WebView2 on Windows, Xcode command-line
  tools on macOS, or WebKitGTK development packages on Linux.

The terminal panel has the extra requirements listed under
[Platform prerequisites](#platform-prerequisites) below.


## Development

Start the desktop shell:

```text
cd apps/desktop
pnpm install
pnpm tauri dev
```

Enter a workspace path in the shell and select Connect. The default `auto`
endpoint selects a stable loopback address for that workspace; Tauri reuses a
healthy Harness runtime there or starts one in the desktop backend process. You
can enter a fixed address such as `127.0.0.1:4545` to connect to a manually
managed endpoint. No separate RPC server command is normally needed. The
conversation panel shows prompts, streamed assistant output, and
run phase. The context panel shows expandable tool cards, verification results,
a checkpoint timeline, and a chronological session timeline. Approval requests
are explicitly allow-once or deny-once; no permanent policy change is inferred
by the UI. Select a persisted session and use Resume to rehydrate its durable
events and conversation, then continue sending work. Closing the window does
not corrupt the runtime-owned session.

## Code changes and checkpoints

The Changes view is a read-only code observability surface built on Monaco
Editor:

- Changed files grouped into added, modified, and deleted, with per-file
  additions and deletions.
- Per-file diffs in Monaco with a split/inline toggle, plus a plain source
  viewer for files without a before/after.
- Syntax highlighting from a locally bundled set of tokenizers. Monaco ships
  from the local bundle (not a CDN) and its language *services* are not loaded,
  so the app works fully offline and the shell stays fast to open.
- A checkpoint timeline listing checkpoint ID, timestamp, the triggering task,
  and affected files, with a Restore control per checkpoint.

Restore calls `checkpoint.undo` on the runtime. All restore safety behavior —
scoping to the files the checkpoint recorded, detecting conflicts, and leaving
unrelated working-tree changes alone — lives in the Rust runtime. The frontend
issues no filesystem writes.

The changes view is driven by runtime events: it refreshes when a run reports a
file change, when a mutating tool completes, and when a run ends or a
checkpoint is restored. Use the refresh control to re-read on demand. The
runtime's own `.cogito/` state is excluded from reported code changes.

## Terminal

The terminal panel opens an interactive shell backed by a runtime pseudo-terminal
and rendered with xterm.js. Output streams in both directions, resizes are
forwarded to the PTY, Ctrl+C is delivered as a real interrupt, and closing the
shell reports its exit.

### Human vs agent command execution

These are two different paths and the panel labels itself accordingly
("human · not policy governed"):

- **Agent commands** go through the runtime's tool registry, which checks every
  call against the policy engine and runs it captured and non-interactively.
- **Terminal sessions** are interactive and intentionally skip agent policy,
  because a person types into them directly. The runtime enforces the split:
  `harness-pty` registers no tool, so the agent cannot reach it, and
  `terminal.open` requires `origin: "human"`.

A terminal is tied to the current workspace and is closed when the workspace
changes or the runtime connection drops. The runtime reaps a client's terminals
on disconnect, so closing the window does not leave a shell running.

### Platform prerequisites

PTY support uses the platform's native pseudo-terminal layer, so there are extra
requirements beyond the Tauri prerequisites above:

- **Windows 10 version 1809 (build 17763) or newer** is required for ConPTY,
  which the runtime uses to back the shell. Older builds cannot open a terminal.
  The default shell is PowerShell (`powershell.exe`); it is present on all
  supported Windows versions.
- **Linux** requires a PTY-capable environment; the default shell is `/bin/sh`.
  `portable-pty` needs no extra packages, but a headless container without
  `/dev/ptmx` cannot create a terminal.
- **macOS** uses the native `forkpty` implementation and has no extra
  requirements.

The console host inside a Windows PTY queries the terminal for its cursor
position and blocks until something answers. xterm.js answers this
automatically, which is why the terminal must be rendered with a real terminal
emulator rather than printed to a log view.

## Settings

Open settings from the gear button in the top bar. Five screens are available:
Models, Runtime, Permissions, Project, and Verification. Values are read from and
applied through the runtime; the desktop never edits configuration itself.

Model and permission changes are validated by the runtime before anything is
applied, so an invalid value is rejected and the previous setting stays in force.

### Policy rules

The Permissions screen lists the rules it finds in `.agent/policy.toml` at the
repository root, using the same schema as the CLI's `.agent/config.toml`. In v0
the development runtime builds its policy engine from the execution mode alone, so
those listed rules are shown for reference but are not loaded into the engine that
authorises tool calls. The two built-in protections, the workspace boundary and
credential-path denial, are enforced by the engine itself and always apply. The
CLI does load and enforce configured rules.

### API keys

API keys are read from the runtime process environment and are never sent to the
desktop. The Models screen shows only whether a credential is configured and
which environment variable holds it; the value is never displayed, editable, or
logged. The runtime keeps no key in its configuration and does not implement any
home-grown encryption. You can enter a key in **Settings → Models → Provider
connections**, or start the desktop process with a provider key in its environment.
For Anthropic, for example:

```powershell
$env:ANTHROPIC_API_KEY = "<your-key>"
$env:COGITO_MODEL_PROVIDER = "anthropic"
$env:COGITO_MODEL = "claude-sonnet-4-6"
pnpm tauri dev
```

For OpenAI, use `OPENAI_API_KEY` and `COGITO_MODEL_PROVIDER=openai` before
launching the app.
The runtime defaults Anthropic requests to `https://api.anthropic.com/v1` and
reads `ANTHROPIC_API_KEY`; `COGITO_MODEL_BASE_URL` and
`COGITO_MODEL_API_KEY_ENV` override those defaults.

| Variable | Purpose |
| --- | --- |
| `COGITO_MODEL_PROVIDER` | `mock`, `openai`, or `anthropic` |
| `COGITO_MODEL` | Model name |
| `COGITO_MODEL_API_KEY_ENV` | Variable holding the API key (`OPENAI_API_KEY` or `ANTHROPIC_API_KEY` by provider) |
| `COGITO_MODEL_BASE_URL` | Provider API root (`https://api.openai.com/v1` or `https://api.anthropic.com/v1` by provider) |
| `RUST_LOG` | Runtime log level |

## Design system

The interface is styled from a single token layer. `src/styles.css` declares
every colour as a CSS custom property in the `:root` block, and
`tailwind.config.js` maps semantic names onto those variables:

| Group | Tokens |
| --- | --- |
| Surfaces | `app`, `panel`, `elevated`, `overlay`, `sunken`, `hover`, `active`, `scrim` |
| Borders | `line`, `line-strong`, `line-stronger` |
| Text | `primary`, `secondary`, `muted`, `faint`, `inverse` |
| Status | `accent`, `success`, `warning`, `error`, `info` |

Colours are stored as bare RGB channels rather than hex so Tailwind's
`/opacity` modifier keeps working (`bg-panel/80`). A `[data-theme="light"]` block
declares the same tokens for a light palette; it is not selectable in v0, but
because nothing is hardcoded, enabling it later is a data-attribute change rather
than a redesign.

Radius, motion, shadow, and focus-ring values are tokens in the same file, and the
icon sizes are spacing keys so the `size-*` utility resolves them.

**Use the tokens.** A component writes `bg-panel` and `text-muted`, never a hex
value or a hand-tuned grey. `pnpm check:classes` fails the build step if a class
used in a component does not resolve to a rule in the generated CSS, which
catches the case where a token refactor silently unstyles an element.

**Use the primitives.** `src/components/ui/` exports `Button`, `IconButton`,
`Tooltip`, `Badge`, `Separator`, `StatusIndicator`, `Panel`, `ScrollArea`,
`Popover`, `Dropdown`, `ContextMenu`, `Modal`, and `CommandMenu`, plus `toneText`
and friends for mapping a status onto a colour. They own the shared behaviour:
Escape and outside-click dismissal, focus restoration, the focus trap in
`Modal`, and the keyboard contract for menus. Re-implementing that in a component
is how the interface ends up inconsistent.

**Monaco and xterm** cannot use Tailwind classes, so they read the same custom
properties at runtime through `src/lib/tokens.ts` (`color`, `alpha`, `hex`).
That keeps one palette rather than three. If you add a token and need it in the
editor or the terminal, add it to the `FALLBACK` map there too.

## Checks

```text
cd apps/desktop
pnpm typecheck
pnpm lint
pnpm test
pnpm build
pnpm check:classes
```

`pnpm verify` runs all five. To compile the native shell as well:

```text
pnpm tauri build --debug
```

`bundle.active` is `false` in `tauri.conf.json`, so this produces an executable
under `target/debug/` rather than a packaged installer.

The v0 transport is loopback-only and unauthenticated. Do not bind it to a
public interface.
