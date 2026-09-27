# CogitoAI

CogitoAI is a model-agnostic coding-agent harness. It gives an agent a controlled
set of tools over a local workspace, records everything the agent does as an
append-only event log, and lets you inspect, verify, and undo the result. When the
workspace is a Git repository, recovery is precise and Git-aware.

It ships as two clients over one runtime: a terminal CLI and a Tauri desktop
application.

## Project overview

### What the harness does

You give it a task in a repository. It assembles context from the project, asks a
model what to do, executes the model's tool calls under a policy engine, records
every step as a durable event, runs the project's own verification commands, and
reports what changed. You approve anything the policy says requires approval, and
you can put the repository back the way it was.

### Architectural philosophy

**Model agnostic.** The agent loop never learns which vendor it is talking to.
`harness-models` defines messages, tool calls, streaming deltas, usage, and
capabilities; a provider is anything that implements `ModelProvider`. Swapping
models changes a configuration value, not the architecture. A deterministic mock
provider is a first-class provider, not a test stub bolted on afterwards, which is
what makes the whole system testable without a network.

**Local first.** The runtime runs in-process on your machine and the RPC transport
binds to loopback. There is no cloud service, no account, and no telemetry.
Credentials are read from the environment of the process you started and are never
sent to a client, written to a config file, or logged. Your code does not leave the
machine.

**Deterministic execution control.** The model's freedom ends at the tool
boundary. A model may only ask for an action; the runtime decides whether that
action happens. Every call is matched against the policy engine, which resolves to
allow, ask, or deny from the mode, the configured rules, and the path or command
involved. Reads and searches are bounded, writes are checkpointed, commands have a
timeout and an output cap, and a cancelled or failed run still closes its session
cleanly. The same task produces the same event stream whether a person or a script
is watching.

**CLI and desktop over one runtime.** Both clients drive the same agent core, the
same policy engine, and the same session store. The CLI composes them in-process;
the desktop reaches them across a versioned JSONL RPC protocol. Neither client
contains agent logic, so a capability added to the runtime is available to both,
and the desktop cannot obtain a privileged capability the CLI could not, because
`harness-rpc` is the only thing the shell links.

**Event-sourced sessions.** A session is a JSONL file with one schema-versioned
event per line. Nothing is mutated in place: tool calls, approvals, verification
results, checkpoints, and compaction are all appended. Reconstructing a session
means replaying its log, so history stays auditable and a crash can lose at most
the final partially written record, which is then reported as a warning rather
than silently dropped.

**Git-aware recovery.** Checkpoints are captured against the repository's own
state, so undo is precise. A restore touches only the files the checkpoint
recorded, refuses to proceed when a file changed after the checkpoint was taken,
and leaves unrelated work, untracked files, and the index alone. The agent works
with your dirty working tree rather than requiring a clean one.

## Architecture

```text
        CLI (harness-cli) ──────────────┐
                                       ├── agent core (harness-agent)
        Desktop (Tauri/React) ── RPC ──┘        │
        (harness-rpc, versioned JSONL           │
         over loopback TCP)                     ▼
                              ┌──────────────────────────────┐
                              │ tools · policy · models ·    │
                              │ session · git · context ·    │
                              │ pty · verification · core    │
                              └──────────────────────────────┘

   The CLI composes these crates in-process; the desktop reaches them only
   through the RPC runtime. Neither client contains agent logic.
```

Both clients are untrusted with respect to privileged operations. A client may
request; the runtime decides. Every privileged call is evaluated by the same
`harness-policy` engine against the open workspace before it happens, and the
desktop can reach nothing without going through `harness-rpc`, which links no tool,
provider, or policy implementation of its own.


See [`docs/architecture.md`](docs/architecture.md) for crate responsibilities and
dependency direction, and [`docs/development.md`](docs/development.md) for a crate
map and step-by-step guides to adding a provider, tool, policy rule, or client
surface.

## Prerequisites

| Requirement | Version | Notes |
| --- | --- | --- |
| Rust | 1.78 or newer (`rust-version` in the workspace manifest) | Verified on 1.95.0 |
| Node.js | 20 or newer | Verified on 24.20.0; only the desktop app needs it |
| pnpm | 10 or newer | Verified on 10.33.4; the committed `pnpm-lock.yaml` pins resolutions |
| Git | 2.x recommended | Required for checkpoints and diffs; the agent still runs without it |
| Platform C++ toolchain | see below | Required by Tauri only |

Tauri 2 platform prerequisites, needed only for the desktop application:

- **Windows:** Microsoft WebView2 Runtime, plus Visual Studio Build Tools with the
  desktop C++ workload. The interactive terminal additionally needs Windows 10
  version 1809 (build 17763) or newer for ConPTY.
- **macOS:** Xcode command-line tools.
- **Linux:** the WebKitGTK development packages required by Tauri 2. A headless
  container without `/dev/ptmx` cannot open an interactive terminal.

## Installation

From a clean clone:

```bash
git clone https://github.com/OctranTechnologies/CogitoAI.git
cd CogitoAI
cargo build --workspace
```

That is enough for the CLI. The desktop application additionally needs the
frontend dependencies:

```bash
cd apps/desktop
pnpm install
cd ../..
```

pnpm 10 blocks dependency lifecycle scripts by default and prints an
"Ignored build scripts" warning for `esbuild`. That warning is expected here and
does not need `pnpm approve-builds`; the build works because those packages ship
prebuilt binaries.

## Model configuration

The harness runs without any credentials. The default provider is a deterministic
mock that executes a fixed, scripted workflow through the real agent loop, policy
engine, checkpoint store, and verification commands, so you can exercise the whole
system offline.

To use a real provider, set the API key in the environment of the process you
start. Never put a key in a file, and never commit one.

```bash
# PowerShell
$env:OPENAI_API_KEY = "<your-key>"
$env:COGITO_MODEL_PROVIDER = "openai"
$env:COGITO_MODEL = "gpt-4o-mini"
```

```bash
# bash
export OPENAI_API_KEY="<your-key>"
export COGITO_MODEL_PROVIDER="openai"
export COGITO_MODEL="gpt-4o-mini"
```

| Variable | Purpose | Default |
| --- | --- | --- |
| `COGITO_MODEL_PROVIDER` | `mock` or `openai` | `mock` |
| `COGITO_MODEL` | Model name passed to the provider | provider default |
| `COGITO_MODEL_API_KEY_ENV` | Name of the variable holding the key | `OPENAI_API_KEY` |
| `COGITO_MODEL_BASE_URL` | Provider base URL, for OpenAI-compatible endpoints | provider default |
| `COGITO_MOCK_REPAIR` | Mock-only test hook; see [Testing the repair loop](#testing-the-repair-loop) | unset |
| `RUST_LOG` | Runtime log level for the RPC runtime | `info` |

The key is read from the environment only. It is never returned by the settings
API, never placed in an RPC frame, and never logged; clients are told only whether
a credential is present and which variable holds it. The harness stores no key on
disk and implements no home-grown encryption.

## Current v0 capabilities

Everything in this list is implemented and covered by tests.

**Agent and tools**

- Single-agent loop with tool calling, streaming deltas, and usage accounting.
- Seven tools: `read_file`, `write_file`, `apply_patch`, `list_directory`, `glob`,
  `grep`, and `shell`.
- Path containment: every tool resolves paths against the workspace root, so `..`
  segments, absolute paths, and symlinks cannot escape it.
- Bounded reads and command execution, with per-command timeouts.

**Policy and approvals**

- Four execution modes: `read-only`, `safe`, `normal`, `auto`.
- Per-request decisions of allow, ask, or deny, from the mode, configured rules,
  and the path or command involved.
- Explicit deny of credential-bearing paths (`.env`, `.ssh/`, private keys, and
  certificate files) in every mode, including `auto`.
- Interactive approvals over the CLI and the desktop, with cancellation that
  always takes effect and unanswered prompts that time out.

**Verification and recovery**

- Verification plans derived from detected manifests, with formatter, lint,
  typecheck, build, targeted test, general test, and Git-diff steps.
- Failed verification is fed back into the model, so the agent can correct its own
  work and re-verify.
- Git-aware checkpoints and precise, conflict-refusing undo.
- Compaction of long sessions with the full history preserved.

**Clients**

- CLI with JSONL event output, session inspection, resume, status, diff, and undo.
- Tauri desktop with conversation, tool activity, verification results, approvals,
  cancellation, Monaco source and diff views, checkpoint timeline, an interactive
  terminal, and five settings screens.
- Versioned RPC runtime with a loopback JSONL protocol.

### Design system

The desktop uses a token-driven dark theme built for density. Colours live only
in `src/styles.css` as CSS custom properties, and Tailwind maps semantic names onto
them, so a component writes `bg-panel` or `text-muted` and never a literal value.
A light theme is declared but not selectable in v0, which keeps every component
legible under either palette.

`src/components/ui/` holds the primitives: `Button`, `IconButton`, `Tooltip`,
`Badge`, `Separator`, `StatusIndicator`, `Panel`, `ScrollArea`, `Popover`,
`Dropdown`, `ContextMenu`, `Modal`, and `CommandMenu`. Monaco and xterm are
configured from JavaScript and so read the same custom properties at runtime
through `src/lib/tokens.ts` rather than keeping a second copy of the palette.

## Running the CLI

Inspect a workspace without running any project code:

```bash
cargo run -p harness-cli -- .
```

Run the deterministic mock workflow, which needs no credentials:

```bash
cargo run -p harness-cli -- --yes run "Create a mock output"
```

Use a real provider:

```bash
cargo run -p harness-cli -- --model-provider openai --model gpt-4o-mini run "Inspect and improve the project"
```

Point at another repository, with machine-readable output:

```bash
cargo run -p harness-cli -- --workspace /path/to/project --json run "Fix the failing test" /path/to/project
```

Session and recovery commands:

```bash
cargo run -p harness-cli -- sessions
cargo run -p harness-cli -- session inspect <session-id>
cargo run -p harness-cli -- resume <session-id> "Continue the remaining work"
cargo run -p harness-cli -- status
cargo run -p harness-cli -- diff
cargo run -p harness-cli -- undo
```

`inspect` reports the repository root, Git state, detected languages, manifests,
package manager, monorepo indicators, instruction files, and likely verification
commands:

```bash
cargo run -p harness-cli -- inspect --json
```

Global options: `--workspace` (default `.`), `--session-root` (default
`.cogito/sessions`), `--model-provider`, `--model`, `--json`, `--yes`,
`--log-level`, `--compaction-threshold`. Agent runs are interruptible with
`Ctrl+C`; the cancellation token is shared with tool execution and verification
commands.

`--yes` auto-approves policy prompts. Use it only in a disposable workspace or in
CI.

### Interactive terminal

Start the full-screen terminal interface in a terminal:

```bash
cargo run -p harness-cli -- tui
```

The header shows the harness version, configured model and provider, and workspace.
Type a task and press Enter, or use `/help` for session, inspection, recovery, and
model commands. `/run <task>` starts work and `/resume <session-id> [task]`
continues a saved session. Approval prompts accept `Y` or `N`. The activity feed
shows concise tool and verification events; commands such as `/inspect` temporarily
restore the normal terminal so their complete output remains available in scrollback.
The latest activity is printed again when you leave the interface. A compact
status line at the bottom shows the model, active execution mode, repository branch
and worktree state, reported token totals when a provider supplies reliable usage,
and elapsed time for the current run. Its second line shows an approval, current
action, or shortcuts. Context percentage, price, and reasoning effort are omitted
when the runtime cannot determine them.

The interactive slash commands are `/help`, `/run`, `/resume`, `/inspect`,
`/sessions`, `/status`, `/diff`, `/undo`, `/config`, `/model`, `/mode`, `/clear`,
`/cancel`, and `/exit`. `/mode` reports the execution mode loaded from the
workspace policy; change that setting in `.agent/config.toml` and restart the CLI.
Tab completes commands. The `TERM=dumb` line-oriented fallback lists only the
commands it supports while preserving ordinary terminal output.

`Alt+Enter` (or `Ctrl+J`) inserts a line break; Enter submits. Up/Down navigate
command history for single-line input, and Tab completes slash commands. PageUp and
PageDown scroll the activity feed. While work is running, `Ctrl+C` cancels it. At
an idle prompt, `Ctrl+C` clears a draft and exits when the prompt is empty; `Ctrl+D`
exits from an empty prompt. `Esc` clears the current draft.

On terminals reporting `TERM=dumb`, the CLI uses a line-oriented fallback. It exits
after running a task so Ctrl+C cancellation remains reliable. The full-screen UI
requires terminal stdin and stdout; for pipes, scripts, and automation, keep using
the existing commands such as `harness run` and `--json`. The interactive UI cannot
be combined with `--json`.

## Running the desktop application

The desktop is a client: it does not start or own the runtime, so run the
development runtime first. In one terminal, from the repository root:

```bash
cargo run -p harness-rpc --bin cogito-rpc-dev -- . 127.0.0.1:4545
```

The first argument is the workspace to open and the second is the listen address;
both have defaults, so `cargo run -p harness-rpc --bin cogito-rpc-dev` alone is
equivalent to the line above. Sessions and checkpoints for that workspace are
written under its `.cogito/` directory. In a second terminal:

```bash
cd apps/desktop
pnpm install
pnpm tauri dev
```

`pnpm tauri dev` starts the Vite dev server itself and compiles the Rust shell. In
the app, enter the runtime address and a repository path, then select Connect.

The desktop command palette opens with `Ctrl+K` or `Ctrl+Shift+P` (use `Cmd` on
macOS). It can create a task, open a project, resume a session, switch model or
execution mode, show diffs or checkpoints, open the terminal, undo the latest
checkpointed harness change, and open settings. `Ctrl/Cmd+N` starts a task,
`Ctrl/Cmd+P` focuses project and session search, `Ctrl/Cmd+Enter` submits the
composer, and `Ctrl/Cmd+\`` toggles the terminal. `Escape` closes open overlays.

To build the desktop binary without bundling installers:

```bash
cd apps/desktop
pnpm tauri build --debug
```

Installer bundling is disabled in `tauri.conf.json` (`bundle.active` is `false`),
so this produces an executable at `<repo-root>/target/debug/cogitoai-desktop`
rather than a packaged installer. The interactive terminal requires a real terminal
emulator to answer the console host's cursor-position query, which xterm.js does;
the CLI's non-interactive shell tool does not.

See [`apps/desktop/README.md`](apps/desktop/README.md) for the desktop-specific
behaviour, including the human-versus-agent terminal boundary.

## Running tests

Rust, from the repository root:

```bash
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

Frontend, from `apps/desktop`:

```bash
cd apps/desktop
pnpm typecheck
pnpm lint
pnpm test
pnpm build
pnpm check:classes
```

`pnpm verify` runs all five in order. `check:classes` asserts that every static
class name used in a component resolves to a rule in the generated CSS, because a
Tailwind class that does not resolve leaves an element silently unstyled rather
than failing the build.

The frontend suite covers the runtime-facing logic (store, events, settings,
terminal, RPC parsing), the design tokens, the UI primitives, and a render smoke
test of the whole shell. The Rust suite includes end-to-end tests that drive the
real components rather than mocks of them: the full agent lifecycle against a
real Git repository, a security audit of the workspace and permission boundaries,
the CLI binary against a real Cargo project where a broken edit genuinely fails
`cargo test`, and the RPC runtime over a real socket.

## Configuration

Project-local configuration is optional. All files live under `.agent/` in the
repository root.

`.agent/config.toml` sets the package manager and verification commands:

```toml
package_manager = "pnpm"

[commands]
test = ["pnpm", "test"]
build = ["pnpm", "run", "build"]
format = ["pnpm", "run", "format"]
lint = ["pnpm", "run", "lint"]
typecheck = ["pnpm", "run", "typecheck"]
```

The same file may carry a `[policy]` section, which the CLI loads into its policy
engine:

```toml
[policy]
mode = "normal"

[[policy.rules]]
name = "deny-credentials"
action = "deny"
paths = [".env*", ".ssh/**", "**/*.key"]

[[policy.rules]]
name = "ask-generated-files"
action = "ask"
paths = ["generated/**"]

[[policy.rules]]
name = "allow-read-only-tools"
action = "allow"
tools = ["read_file", "grep", "glob", "list_directory"]
operations = ["read", "search"]
```

A rule accepts `name`, `action` (`allow`, `ask`, or `deny`), `priority`, and the
optional matchers `tools`, `operations`, `modes`, `paths`, and
`command_patterns`. Paths are globs relative to the repository root.

`.agent/policy.toml` uses the same schema and is what the desktop Permissions
screen reads to list your configured rules. See
[Policy rule enforcement](#policy-rule-enforcement) for how the two clients differ.

`.agent/instructions.md` adds project instructions to the assembled context.
`AGENTS.md`, `CLAUDE.md`, `README.md`, and `CONTRIBUTING.md` are also read, in that
precedence order, earliest first. Discovery reads filesystem metadata and
read-only Git metadata only; it never runs project scripts.

A malformed configuration file produces a typed configuration error rather than
being silently ignored.

### Policy rule enforcement

This differs between the two clients in v0, and it is worth being explicit:

- The **CLI** loads both the mode and the rules from `.agent/config.toml` and
  enforces them. If that file is absent it runs in `normal` mode with no rules.
- The **desktop** enforces the execution mode, which is runtime state you can
  change in the Permissions screen without editing a file. The development
  runtime constructs its policy engine from the mode alone, so the rules in
  `.agent/policy.toml` are listed in the Permissions screen but are not loaded
  into the engine that authorises tool calls.

The two built-in protections described under [Permissions](#permissions) — the
workspace boundary and credential-path denial — are enforced by the engine
itself and therefore apply in both clients regardless of configuration.

## Session storage

Sessions are stored as JSONL under `.cogito/sessions/` in the workspace, one file
per session named `<session-id>.jsonl`, with one schema-versioned event per line.
Checkpoints are stored separately under `.cogito/checkpoints/` at the repository
root.

The CLI's session root is configurable with `--session-root`. The RPC runtime
defaults to `<workspace>/.cogito/sessions`.

`.cogito/` is runtime state, not user code, and is excluded from reported code
changes. It is already listed in `.gitignore`.

A truncated final record, which is what a crash mid-append leaves behind, is
reported as a load warning while all earlier events remain intact. A malformed
record anywhere earlier fails validation rather than being skipped, because
silently dropping it would rewrite history. `resume` appends an explicit
`session.resumed` event and reactivates the same session, including one that
previously completed or failed.

## Permissions

Every privileged operation resolves to one of three decisions:

- **allow** — run it without asking.
- **ask** — stream an approval request and wait for an explicit allow-once or
  deny-once answer.
- **deny** — refuse it and tell the model why.

The execution mode sets the default, and configured rules override it. An explicit
`deny` rule always wins; otherwise the highest-priority matching rule wins, and
mode defaults apply when no rule matches.

| Mode | Reads and searches | Writes and patches | Commands |
| --- | --- | --- | --- |
| `read-only` | allow | deny | deny |
| `safe` | allow | ask | ask |
| `normal` | allow | allow | allow for known-safe commands, otherwise ask |
| `auto` | allow | allow | allow for known-safe commands, otherwise ask, with explicit denies preserved |

Two built-in protections apply in every mode, including `auto`, and are not
overridable by a rule that merely asks:

- The workspace is a hard boundary. Paths resolving outside it are denied.
- Credential-bearing paths are denied outright rather than gated on approval, so
  a mistaken "allow" cannot expose them. The protected set is `.env`, `.env.*`,
  anything under `.ssh/`, `id_rsa`, `id_ed25519`, `id_ecdsa`, and files ending in
  `.pem`, `.key`, `.p12`, or `.pfx`.

The shell tool cannot be path-checked the way file tools are, because a command is
an arbitrary string. `cat .env` is therefore evaluated as a command and prompts,
rather than being denied by path. Approval is the mitigation.

The engine's configured mode is authoritative. A request cannot widen the
permission level you selected.

## Checkpoints and undo

A checkpoint records the files the agent is about to change, together with their
contents at that moment. `undo` restores the most recent checkpoint, or one chosen
by ID.

Undo restores only recorded paths to their recorded baseline. It preflights those
paths and aborts without changing anything when their current contents no longer
match what the harness left behind, so a formatter or a person who edited the file
afterwards is never silently overwritten. The conflict is reported rather than
resolved. Unrelated user edits, untracked files, staged index state, and
unrelated history are left untouched, and no commit is created or rewritten.

Deliberate limitations:

- Checkpoints are shadow snapshots, not commits. A harness change that was not
  recorded with `record_harness_change` cannot be safely attributed or undone, so
  callers must record each mutation immediately.
- Files over 10 MiB, symlinks, and unsupported Git states are not snapshotted.
- A dirty working tree is supported. A checkpoint captures the dirty state, so
  undo returns the tree to what it was before the run rather than to `HEAD`.
- Verification steps that reformat files, such as `cargo fmt`, can change a file
  after its checkpoint was taken, which makes a later undo of that file report a
  conflict instead of proceeding.

## Testing the repair loop

The mock provider can be pointed at a specific break-and-fix cycle, which is how
the corrective path is tested without a model or a network. `COGITO_MOCK_REPAIR`
takes JSON with a `path`, the `broken` content to write first, and the `fixed`
content to write after verification fails:

```bash
# PowerShell
$env:COGITO_MOCK_REPAIR = '{"path":"src/lib.rs","broken":"pub fn value() -> u32 { oops","fixed":"pub fn value() -> u32 { 2 }"}'
cargo run -p harness-cli -- --json run "make the test pass"
```

```bash
# bash
export COGITO_MOCK_REPAIR='{"path":"src/lib.rs","broken":"pub fn value() -> u32 { oops","fixed":"pub fn value() -> u32 { 2 }"}'
cargo run -p harness-cli -- --json run "make the test pass"
```

This hook affects the mock provider only and is ignored by every real provider. It
writes real files, so point it at a scratch repository.

## Project status

This is v0. The following are **not** implemented, and no part of this repository
should be read as promising them:

- **No Jev decision layer.** There is no separate decision, planning, or
  adjudication stage. A single agent loop runs from task to completion; the
  verification-correction loop is the only structured retry.
- **No subagents.** There is no multi-agent orchestration, delegation, or
  parallelism. `harness-agent` runs exactly one agent per task.
- **No MCP.** There is no Model Context Protocol client or server, and no external
  tool server integration. Tools are compiled-in Rust implementations.
- **No plugin system.** There is no runtime tool or provider discovery, and no
  third-party extension loading. Providers and tools are registered in Rust at
  build time. The `@tauri-apps/plugin-dialog` dependency is Tauri's own native
  file dialog, not a CogitoAI extension point.
- **No remote execution.** The runtime runs in-process on the local machine. There
  is no container sandbox, no SSH or remote host execution, and no distributed
  scheduling.
- **No authentication.** The RPC protocol is unauthenticated and grants full
  control of the opened workspace. It binds to loopback and must not be exposed to
  a network.
- **No installer bundles.** Tauri bundling is disabled; only the executable is
  produced.
- **Configured policy rules are CLI-only in v0.** The CLI loads mode and rules
  from `.agent/config.toml` and enforces both. The desktop enforces the execution
  mode but lists rules without loading them into the engine that authorises tool
  calls. See [Policy rule enforcement](#policy-rule-enforcement).
- **Verification is command-based.** Verification runs the project commands that
  were discovered or configured. There is no semantic analysis, test selection, or
  understanding of which failure matters.

## License

MIT. See [`LICENSE`](LICENSE).
