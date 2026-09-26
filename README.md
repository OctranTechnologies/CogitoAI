# CogitoAI

CogitoAI is a model-agnostic coding-agent harness. The repository currently
contains the Rust workspace, runtime contracts, dependency boundaries, and
side-effect-free project discovery and configuration loading.

## Repository layout

- `crates/`: Rust backend and client crates.
- `apps/desktop/`: Tauri 2 + React + TypeScript desktop shell over `harness-rpc`.
- `docs/`: architecture and development documentation.
- `examples/`: small, future-facing usage examples.

## Rust workspace

Requirements: Rust 1.78 or newer.

```text
cargo build --workspace
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

## Running the CLI

Build the binary and run commands from a clean checkout with `cargo run`:

```text
cargo build --workspace
cargo run -p harness-cli -- .
cargo run -p harness-cli -- run "Inspect the project and report the next step"
cargo run -p harness-cli -- sessions
cargo run -p harness-cli -- status
cargo run -p harness-cli -- diff
cargo run -p harness-cli -- config
```

`harness .` inspects the current workspace without running project code. Use a
positional path or the global `--workspace` option for another repository:

```text
cargo run -p harness-cli -- --workspace /path/to/project .
cargo run -p harness-cli -- --workspace /path/to/project run "Fix the failing test" /path/to/project
```

Use `--json` for machine-readable output. Agent runs emit one JSON event per
line, including assistant deltas, tool lifecycle events, approvals,
verification results, and terminal session events. `--yes` auto-approves
policy prompts for non-interactive or CI workflows.

```text
cargo run -p harness-cli -- --json sessions
cargo run -p harness-cli -- --json run "Summarize this repository"
cargo run -p harness-cli -- --json status
```

Agent runs are interruptible with `Ctrl+C`; the cancellation token is shared
with filesystem shell execution and verification commands. A canceled or
failed run is persisted as a session and can be resumed.

The deterministic mock provider is the default and runs a complete scripted
workflow (list directory, write a file, and finish) without credentials:

```text
cargo run -p harness-cli -- --yes run "Create a mock output"
```

To use OpenAI or another compatible endpoint:

```text
$env:OPENAI_API_KEY="<your-key>"
$env:COGITO_MODEL_PROVIDER="openai"
$env:COGITO_MODEL="gpt-4o-mini"
cargo run -p harness-cli -- --model-provider openai --model gpt-4o-mini run "Inspect and improve the project"
```

Session and recovery commands:

```text
cargo run -p harness-cli -- sessions
cargo run -p harness-cli -- session inspect <session-id>
cargo run -p harness-cli -- resume <session-id> "Continue the remaining work"
cargo run -p harness-cli -- status --session <session-id>
cargo run -p harness-cli -- diff
cargo run -p harness-cli -- undo
```

`undo` restores the latest harness checkpoint, or a checkpoint selected by ID,
and refuses to overwrite unrelated user changes. Agent mutations are recorded
in checkpoint snapshots when the workspace is a Git repository.



```text
cargo run -p harness-cli -- inspect
cargo run -p harness-cli -- inspect --json
cargo run -p harness-cli -- inspect --path /path/to/project --json
```

`inspect` reports the repository root, Git availability/branch/working-tree
state, detected languages, package manager, manifests, monorepo indicators,
instruction files, and likely test/build/format/lint/typecheck commands. The
JSON form is the serializable `harness_core::WorkspaceDescription` consumed by
future CLI and desktop clients.

Project-local configuration is optional. When present, `.agent/config.toml`
overrides inferred package-manager and command settings:

```toml
package_manager = "pnpm"

[commands]
test = ["pnpm", "test"]
build = ["pnpm", "run", "build"]

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
```

The policy engine reads the `[policy]` section through
`PolicyEngine::from_file`; explicit `deny` rules always win, followed by
priority-ordered rules and then mode defaults. The modes are `read-only`,
`safe`, `normal`, and `auto`; `read-only` allows reads/searches only, `safe`
asks for mutations and commands, `normal` allows project edits and known safe
commands, and `auto` allows configured categories while preserving explicit
denies.

Supported project instruction files are loaded in this deterministic
precedence order: `AGENTS.md`, `CLAUDE.md`, `README.md`, `CONTRIBUTING.md`, and
`.agent/instructions.md`. Malformed `.agent/config.toml` files return a typed
configuration error rather than being ignored.

Discovery reads filesystem metadata and read-only Git metadata only. It never
runs project scripts, package-manager commands, or other project code.

The CLI is a thin presentation layer over the harness session, agent, policy,
tool, verification, and Git capabilities. The desktop application is not
scaffolded or runnable yet. Its future setup and run commands will be added
here when the Tauri application exists.

## Model providers

The model boundary is provider-neutral. `harness-models` defines messages,
content blocks, tool definitions/calls, streaming deltas, usage, finish
reasons, capabilities, and provider errors without exposing provider request
types. The deterministic mock provider is the default; the OpenAI adapter is a
real implementation using `ureq` and an environment-provided API key.

Inspect the selected model and capabilities:

```text
cargo run -p harness-cli -- model-info
cargo run -p harness-cli -- --model-provider mock --model test-model model-info
```

Ask the mock provider, optionally streaming:

```text
cargo run -p harness-cli -- ask "hello"
cargo run -p harness-cli -- ask --stream "hello"
```

For OpenAI, set credentials without placing them in project files:

```text
$env:OPENAI_API_KEY="<your-key>"
$env:COGITO_MODEL_PROVIDER="openai"
$env:COGITO_MODEL="gpt-4o-mini"
cargo run -p harness-cli -- ask "Summarize this project"
```

On POSIX shells, use `export OPENAI_API_KEY=...` and equivalent
`COGITO_MODEL_*` variables. The supported environment settings are
`COGITO_MODEL_PROVIDER`, `COGITO_MODEL`, `COGITO_MODEL_API_KEY_ENV`, and
`COGITO_MODEL_BASE_URL`; CLI `--model-provider` and `--model` flags override
environment selection. `COGITO_MODEL_BASE_URL` can target an OpenAI-compatible
endpoint. API keys are read only at runtime, are not serialized by the model
configuration or event types, and are never written to session JSONL.

## Filesystem tools

`harness-tools` exposes a common JSON-schema-based `Tool` contract and the
initial safe tools: `read_file`, `write_file`, `apply_patch`, `list_directory`,
`glob`, and `grep`. Invoke them through `ToolRegistry`; the registry checks
policy and emits `tool.requested`, `tool.approved`/`tool.denied`,
`tool.started`, `tool.output`, and `tool.completed`/`tool.failed` events.

All paths are relative to the supplied workspace and are canonicalized before
use. Absolute paths, traversal, and symlink escapes are rejected. Text files
are limited to 256 KiB, directory/search results are bounded and concise, and
`apply_patch` requires exactly one matching `old_text` context before writing.
Binary files are rejected rather than returned as model input.

## Shell execution

The `shell` tool runs explicitly approved commands through a replaceable
`ProcessRunner`; the current implementation uses the platform shell. The
command is selected through the `shell` argument (`auto`, `bash`, `sh`, or
`cmd`), with `working_directory` constrained to the workspace. `timeout_ms`
defaults to 30 seconds and is capped at 10 minutes. Captured stdout/stderr are
bounded to 1 MiB by default/request limits, streamed to the caller, and
reported with exit status.

`ShellTool` exposes a `CancellationToken`; cancellation and timeout paths
terminate the child and best-effort process tree, then join output readers.
There is no auto-approval: the registry requires
`Permission::ExecuteCommand` policy approval for every shell invocation.
Commands still run with the host account’s OS permissions, so workspace path
validation is not a sandbox; Docker, SSH, and remote sandbox runners can be
added behind `ProcessRunner` without changing the agent loop. Windows uses
`cmd /C` by default and Unix uses `sh -lc`; process-tree cleanup depends on
available OS process controls.

## Context assembly

`harness-context` assembles provider-neutral context from system instructions,
workspace metadata, precedence-ordered project instructions, Git state, the
current request, recent conversation, explicitly selected files, and explicitly
provided tool results. It never scans or loads an entire repository and does
not perform semantic/vector search.

`ContextBudget` limits individual files, tool-result contributions, retained
shell output, and the approximate working token budget. `ContextAssembly`
returns the rendered prompt plus every item with its source, inclusion reason,
estimated tokens, original size, inclusion status, and applied limits, making
context decisions inspectable by CLI/RPC/desktop clients.

When the agent's working context reaches its compaction threshold, it derives
or generates a compact continuation state with task, current approach,
discoveries, important files, modified files, decisions, failed attempts, test
status, and remaining work. The state is emitted as `context.compacted`, placed
back into the working context, and never replaces the JSONL event history. The
compaction strategy is replaceable through `CompactionStrategy`, so a future
low-cost model or assistant can produce richer summaries. The default trigger is
24,000 estimated tokens; use the CLI's `--compaction-threshold <tokens>` option
to tune it for a workspace.

## Verification

`harness-verification` turns discovered project commands into structured
verification steps for formatter/check, lint, typecheck, build, targeted tests,
general tests, and final Git diff. A targeted plan is selected after source
changes when a test command is known; broad tests are not run automatically in
that path. Results include command, category, duration, exit code, bounded
output, and relevant diagnostics, and verification failures are added to the
next agent context.

Project-local `[commands]` overrides in `.agent/config.toml` are used before
inferred defaults:

```toml
[commands]
format = ["cargo", "fmt", "--all", "--", "--check"]
lint = ["cargo", "clippy", "--workspace", "--all-targets", "--all-features", "--", "-D", "warnings"]
typecheck = ["cargo", "check", "--workspace"]
build = ["cargo", "build", "--workspace"]
test = ["cargo", "test", "--workspace"]
```

## Desktop application

The initial cross-platform desktop shell lives in `apps/desktop`. It uses
Tauri 2, React, TypeScript, Vite, Tailwind, and Zustand. It is deliberately a
thin client: React owns UI/session state, Tauri commands only bridge the
versioned `harness-rpc` TCP protocol, and the Rust runtime remains the sole
owner of agent execution, tools, policy, approvals, verification, checkpoints,
and durable sessions.

Prerequisites:

- Rust 1.78 or newer and the Rust target required by Tauri for your platform.
- Node.js 20 or newer and pnpm 10 or newer.
- Windows: Microsoft WebView2 Runtime and Visual Studio Build Tools with the
  desktop C++ workload. macOS: Xcode command-line tools. Linux: the WebKitGTK
  development packages required by Tauri 2.
- A running CogitoAI RPC runtime. The desktop does not start or own the runtime
  process.

Install and run the frontend/Tauri shell:

```text
cd apps/desktop
pnpm install
pnpm tauri dev
```

For a local mock runtime, use the development binary from another terminal at
the workspace root:

```text
cargo run -p harness-rpc --bin cogito-rpc-dev -- . 127.0.0.1:4545
```

In the desktop shell, enter the runtime address and a repository path, then
select Connect. The shell can create/resume sessions, send messages, stream
assistant output, show tool activity and verification results, approve or deny
tools once, cancel runs, and inspect the chronological runtime timeline.
Closing and reopening the window does not delete or corrupt an active session:
reconnecting reloads the session list and the runtime remains the source of
truth. Use Resume on a selected session to rehydrate its persisted conversation
and event timeline before continuing.

### Code changes and checkpoints

The Changes view adds code-change observability on top of the runtime. It
lists added, modified, and deleted files with per-file line counts, and opens
any file in Monaco Editor. Diffs render side-by-side (with an inline toggle)
and source files render read-only with syntax highlighting; the shell cannot
edit your files. Monaco is bundled locally, so viewing works with no network
access.

The checkpoint timeline shows each runtime checkpoint with its ID, timestamp,
the task that triggered it, and the files it recorded. Restoring a checkpoint
calls `checkpoint.undo` on the runtime, which applies the existing restore
safety logic: it reverts only the files that checkpoint recorded, refuses to
proceed when a file changed since the checkpoint was taken, and leaves every
other change in the working tree untouched. The desktop never writes to the
filesystem to undo work. The changes view refreshes from runtime events, so it
updates after a run finishes and after a restore.

The runtime keeps its own sessions and checkpoints under `.cogito/` inside the
workspace. Those files are runtime state, not user code, so they are excluded
from reported code changes.

### Terminal

The desktop can open an interactive shell rendered with xterm.js, backed by a
real pseudo-terminal in the runtime. Output streams both ways, resizing is
forwarded to the PTY, and Ctrl+C is delivered as a real interrupt.

Terminals are **human-controlled sessions** and are deliberately separate from
**agent-controlled command execution**:

- Agent commands run through the runtime's tool registry, which evaluates every
  call against the policy engine (deny / ask / allow) and executes it captured
  and non-interactively.
- A terminal is interactive and is intentionally **not** policy-governed, because
  a person is typing into it directly and is responsible for every command.
  Prompting someone to approve their own keystrokes would be noise, not safety.

The boundary is enforced rather than merely documented:

- `harness-pty` exposes no tool implementation and is never registered in a
  `ToolRegistry`, so no model-driven tool call can open or write to a terminal.
- `terminal.open` requires `origin: "human"`; any other origin is refused. The
  origin enum has no agent variant to construct.
- A terminal's working directory must resolve inside the open workspace.

A terminal is bound to its workspace and is closed when the workspace changes or
the connection drops. The runtime also terminates a client's terminals when that
client disconnects, so closing the window never leaves an orphaned shell.

### Settings

The desktop exposes five configuration screens, all served by typed runtime
APIs (`settings.inspect`, `settings.update_model`, `settings.update_permissions`,
`settings.test_model`):

- **Models** — provider, selected model, model capabilities, base URL, and a
  connectivity check. Changing the model or permission mode rebuilds the agent
  runner in place, so a change takes effect on the next run without a restart.
- **Permissions** — the active execution mode, what each operation does when no
  rule matches, the built-in rules, and any rules loaded from
  `.agent/policy.toml`.
- **Project** — workspace path, detected languages and manifests, package
  manager, instruction files, and monorepo status.
- **Verification** — the commands the runtime will run and whether they come
  from project configuration or detection.
- **Runtime** — runtime version, session and checkpoint storage paths, log level
  and destination, and available providers.

### Credentials

API keys are read from the process environment and are **never** returned to the
desktop.

- The runtime reports only whether a credential is available and which
  environment variable holds it (`OPENAI_API_KEY` by default, configurable with
  `COGITO_MODEL_API_KEY_ENV`). There is no field anywhere in the settings
  payload that can carry the key itself.
- Credentials are not stored in runtime configuration, not written to
  configuration files, and not logged. Free-form error text is redacted before it
  is returned to a client.
- No home-grown encryption is used. An unverified cipher is worse than an honest
  environment variable, because it looks like protection without providing it.
- An operating-system keychain backend is not implemented. If one is added it
  will slot in behind the same `SecretStore` trait, and no frontend code changes,
  because clients only ever receive credential *presence*.

Relevant environment variables:

| Variable | Purpose |
| --- | --- |
| `COGITO_MODEL_PROVIDER` | `mock` or `openai` |
| `COGITO_MODEL` | Model name |
| `COGITO_MODEL_API_KEY_ENV` | Name of the variable holding the API key |
| `COGITO_MODEL_BASE_URL` | Provider base URL |
| `OPENAI_API_KEY` | Default API key variable |
| `RUST_LOG` | Runtime log level |

Frontend and Tauri checks:

```text
cd apps/desktop
pnpm typecheck
pnpm lint
pnpm test
pnpm build
pnpm tauri build --debug
```

The production host is expected to embed `RpcServer` or start
`cogito-rpc-dev` for local development. The v0 transport is loopback-only and
has no authentication or encryption; do not bind it to a public interface.


`harness-rpc` provides a versioned, newline-delimited JSON protocol over a
loopback TCP socket. `RpcServer` binds to `127.0.0.1`; `RpcClient` can connect
from the CLI, a desktop host, or an integration test. Requests contain an
explicit protocol `version`, `id`, `method`, and JSON `params`. Responses use
stable `ok`, `result`, and `error.code` fields, while agent progress arrives as
`agent.event` notifications followed by `agent.completed` or `agent.failed`.

The v1 method surface is:

```text
rpc.initialize
workspace.open / workspace.inspect
config.inspect / config.update
session.create / session.list / session.inspect / session.state / session.resume
agent.send / agent.run / agent.approve / agent.deny / agent.cancel
git.status / git.diff
checkpoint.list / checkpoint.inspect / checkpoint.undo
```

`agent.send` and `agent.run` are asynchronous: the server returns a `run_id`
immediately and continues the existing `AgentRunner` on a worker thread. Event
notifications contain the durable `HarnessEvent`; the session JSONL file
remains the source of truth. Approvals are explicit `approval.request`
notifications, and `agent.approve`/`agent.deny` release the waiting agent.
Dropping a client denies pending approvals and cancels the active run so a
worker cannot remain blocked. v0 allows one active agent run per server because
the current event model has no independent run correlation for concurrent
mutations.

There is no standalone daemon binary in v0. Desktop applications and other
frontends embed `RpcServer::bind` (or
`RpcServer::bind_with_approvals` when they need the RPC approval handler) and
own the process lifecycle. The server should be launched by the host with a
fixed loopback address, for example `127.0.0.1:0` to let the OS choose a port,
and the host should report that port to its client. The socket has no
authentication or encryption because it is loopback-only; treat any local
process able to connect as a client of the configured workspace. Do not bind
the v0 server to a public interface, and do not expose provider keys, full
checkpoint contents, or unrestricted tool execution through the protocol.



`harness-git` exposes read-only repository status/diff inspection and
`ShadowCheckpointStore` for recoverable checkpoints. Checkpoints are external
JSON snapshots; the harness never commits, stages, resets, or cleans the user’s
repository automatically.

Create a checkpoint before a mutation, then record each harness-owned path
after the mutation:

```rust
let checkpoint = store.create(&session_id, workspace)?;
fs::write(workspace.join("src/main.rs"), updated_contents)?;
store.record_harness_change(&checkpoint.id, &workspace.join("src/main.rs"))?;
let report = store.undo(&checkpoint.id)?;
```

Undo restores only recorded paths to their checkpoint baseline. It preflights
recorded paths and aborts without changing anything when current contents no
longer match the recorded harness result. Unrelated user edits, untracked
files, staged index state, and unrelated history remain untouched. Files over
10 MiB, symlinks, and unsupported Git states are not snapshotted. Changes made
by the harness but not recorded with `record_harness_change` cannot be safely
attributed or undone; callers must record them immediately after each
mutation. A conflict is reported rather than resolved automatically.

## Session storage

`harness-session` persists execution history as portable JSONL. The host chooses
the session root when constructing `JsonlSessionStore`; each session is stored
as `<session-root>/<session-id>.jsonl`, with one schema-versioned event per
line. `load` reconstructs the complete event history, while `resume` appends
an explicit `session.resumed` event and reactivates the same session, including
sessions that previously completed or failed. `recent` lists session metadata
and compaction counts from the same directory. A truncated final record is
reported as a load warning while all earlier events remain intact; complete
malformed records fail validation and are never rewritten.

`context.compacted` events contain both a human-readable summary and the
structured continuation state. Compaction changes only the working context;
the complete event/session history remains permanently available for
inspection. A repeated compaction creates another event and replaces the latest
working continuation state, so older summaries remain auditable.

Session commands are model-free until a continuation is requested:

```text
cargo run -p harness-cli -- --session-root .cogito/sessions session list
cargo run -p harness-cli -- --session-root .cogito/sessions session inspect <session-id>
cargo run -p harness-cli -- --session-root .cogito/sessions session resume <session-id> "Continue the remaining work"
```

`session list` shows IDs, status, event counts, and compaction counts.
`session inspect` shows session metadata, counts, warnings, and the latest
continuation state. `session resume` reopens the stored workspace and session
and continues from the persisted compacted state; use `session inspect --json`
for the complete machine-readable report.

The event model is provider- and UI-independent. `harness-session::EventBus`
provides in-process live subscriptions for CLI, RPC, and future desktop
clients; durable JSONL remains the portable source of truth.

## Architecture

See [`docs/architecture.md`](docs/architecture.md) for crate responsibilities,
dependency direction, runtime composition, and the privileged-operation
boundary enforced for UI clients.

## Conventions

- Rust code uses the workspace edition, version, lint configuration, and
  shared error/ID types.
- Provider integrations belong in `harness-models`, not in `harness-core`.
- Privileged operations are mediated by `harness-policy` and must be invoked
  through the runtime boundary in `harness-rpc`.
- Session state and events are owned by `harness-session`; Git checkpoints and
  verification are separate capabilities rather than UI responsibilities.
