# CogitoAI

CogitoAI is a model-agnostic coding-agent harness. The repository currently
contains the Rust workspace, runtime contracts, dependency boundaries, and
side-effect-free project discovery and configuration loading.

## Repository layout

- `crates/`: Rust backend and client crates.
- `apps/desktop/`: reserved for a Tauri 2 + React + TypeScript desktop client.
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

Run the CLI and inspect a project without executing project code:

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

The CLI currently validates workspace configuration and exposes discovery; it
does not yet expose a completed agent runtime.

The desktop application is not scaffolded or runnable yet. Its future setup and
run commands will be added here when the Tauri application exists.

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

## Run the agent

Install/build the CLI and configure a real provider:

```text
cargo build --workspace
$env:OPENAI_API_KEY="<your-key>"
$env:COGITO_MODEL_PROVIDER="openai"
$env:COGITO_MODEL="gpt-4o-mini"
```

Open a repository and run a task:

```text
cargo run -p harness-cli -- --workspace . agent --path . "Inspect the project and fix the failing test"
```

The agent builds bounded context, streams assistant output, evaluates tool
calls through policy, prompts for approval on `ASK`, persists every event, and
stops on completion or configured limits. Press `Ctrl+C` to request cancellation.
Session JSONL files are stored under `.cogito/sessions/` relative to the CLI
working directory by default; pass `--session-root <path>` to choose another
location.

## Git checkpoints

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
line. `load` and `resume` reconstruct sessions from the append-only history,
and `recent` enumerates sessions from that same directory. A truncated final
record is reported as a load warning while all earlier events remain intact;
complete malformed records fail validation and are never rewritten.

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
