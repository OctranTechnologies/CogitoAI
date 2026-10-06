# CogitoAI

CogitoAI is a local-first software-engineering coding agent. It works in a
repository through policy-controlled tools, keeps an auditable session history,
verifies changes, and lets you inspect diffs and restore checkpoints. When the
workspace is a Git repository, recovery is Git-aware.

It ships as two clients over one runtime: a terminal CLI and a Tauri desktop
application.

Its central workflow is:

**Understand → plan when needed → search/read → edit → verify → inspect the diff
→ repair if needed → finish.** The runtime keeps this loop in one agent core;
the CLI and desktop are two ways to operate and inspect the same local runtime.

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

**Local first.** A persistent runtime process runs on your machine and the RPC
transport binds to loopback. There is no CogitoAI cloud service or telemetry.
Credentials come from explicitly configured environment variables or the OS
credential manager. They are never returned to a client, written to project
configuration or session history, or included in logs. When you use a hosted
model provider, the prompt and selected workspace context are sent to that
provider to generate a response; review the provider's data handling terms.

**Deterministic execution control.** The model's freedom ends at the tool
boundary. A model may only ask for an action; the runtime decides whether that
action happens. Every call is matched against the policy engine, which resolves to
allow, ask, or deny from the mode, the configured rules, and the path or command
involved. Reads and searches are bounded, writes are checkpointed, commands have a
timeout and an output cap, and a cancelled or failed run still closes its session
cleanly. The same task produces the same event stream whether a person or a script
is watching.

**Task-aware completion.** The runtime persists a `TaskRun` snapshot in the
session event log, including a structured `Goal` and revisioned `ExecutionPlan`:
objective, constraints, acceptance criteria, non-goals, milestones, tasks,
validation, decision notes, and completion condition. Progress snapshots survive
process and desktop restarts. Context compaction carries the objective and current
plan forward, so the model can resume from the next incomplete step without
depending on chat history. Ordinary `Plan:` text cannot replace a saved plan;
revisions require a concrete reason and are bounded. Failed commands and tests
return to the model as errors so it can revise and retry. The runtime does not
mark a task done while recorded failures remain unresolved; repeated identical
calls and repeated failures become `blocked`. A model can request a decision with
`[USER_INPUT_REQUIRED]`.

**Bounded coding context.** The session event log remains the durable history;
each model turn receives a freshly assembled, model-window-aware working context
instead of an indefinitely growing transcript. Current request, saved goal and
plan, project instructions, relevant files, recent failures, and the current Git
diff take priority over old shell output and redundant history. Large tool and
shell results are trimmed or summarized while retaining useful diagnostics. A
structured compaction artifact preserves the goal, decisions, changed files,
tests, failures, tried approaches, and next steps for resume. Task snapshots
record estimated and provider-reported input tokens, reusable context, compaction
count, and repository-retrieval effectiveness where known.

**CLI and desktop over one runtime.** Both clients use the shared loopback RPC
bootstrap for agent runs and drive the same agent core, policy engine, and session
store. They reuse a healthy runtime or start a detached runtime process when none
is available. The server binds an OS-assigned loopback port and publishes a small
per-user metadata record so the other client can find it. The connector verifies
the health protocol, process ID, and runtime instance ID before reuse; a startup
lock prevents simultaneous clients from launching duplicate servers for one
workspace. Closing a client leaves the runtime available to other clients.
Neither client contains the agent loop.

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
        CLI (harness-cli) ──────┐
                                ├── RPC runtime (harness-rpc)
        Desktop (Tauri/React) ──┘     │
          versioned JSONL over       ▼
          loopback TCP        agent core (harness-agent)
                                      │
                              ┌──────────────────────────────┐
                              │ tools · policy · models ·    │
                              │ session · git · context ·    │
                              │ pty · verification · core    │
                              └──────────────────────────────┘

   Both clients use the same connector and RPC runtime for agent runs. If the
   endpoint is down, the connector starts the runtime executable for the client.
```

Both clients are untrusted with respect to privileged operations. A client may
request; the runtime decides. Every privileged call is evaluated by the same
`harness-policy` engine against the open workspace before it happens, and the
desktop can reach nothing without going through `harness-rpc`, which links no tool,
provider, or policy implementation of its own.

File edits use focused create, patch, exact-text/range replacement, delete, and
move operations. Edits are staged atomically, checked against the file revision
the session read, and returned with a bounded unified diff. Exact patches refuse
ambiguous context; UTF-8 BOMs and CRLF/LF endings are preserved. The agent creates
a task checkpoint before editing, so its changes remain available to review and
undo without disturbing unrelated dirty Git work. The runtime also refreshes its
repository index and requests best-effort quick diagnostics when a supported
language server is available.


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

## LLM Providers

Provider support is gated on the deterministic coding-agent conformance fixture,
cross-route error/cancellation checks, and adapter-level protocol tests. The
fixture runs the same read, search, missing-file, patch, test-failure recovery,
and final-response flow through every supported transport route. The shared
conformance suite also checks normalized authentication, invalid-model, and
rate-limit errors plus cancellation on every route; timeout handling is tested
at the shared SSE transport boundary. Live credentials are optional and are not
used in CI.

| Provider | API transport | Credential environment variable |
| --- | --- | --- |
| OpenAI | Responses API | `OPENAI_API_KEY` |
| Anthropic | Messages API | `ANTHROPIC_API_KEY` |
| Google Gemini | Native Generate Content API | `GEMINI_API_KEY` |
| OpenCode Zen | Catalog-selected Responses, Chat Completions, or Messages | `OPENCODE_API_KEY` |
| OpenCode Go | Catalog-selected Responses, Chat Completions, or Messages | `OPENCODE_API_KEY` |

Zen and Go are separate services and catalogs on the same OpenCode account.
Go model access requires the account's Go subscription. OpenCode chooses a
protocol from each model's catalog metadata; it does not assume one protocol
for every model. Available model lists are discovered dynamically and may
change as provider catalogs and account access change.

The harness runs without any credentials. The default provider is a deterministic
mock that executes a fixed, scripted workflow through the real agent loop, policy
engine, checkpoint store, and verification commands, so you can exercise the whole
system offline.

To use OpenAI, Anthropic, Google Gemini, OpenCode Zen, or OpenCode Go, create an
API key with the provider and set it in the environment of the process that
starts Harness. Never put the key in project configuration, a session file, or a
command-line argument.

```bash
# PowerShell
$env:OPENAI_API_KEY = "<your-key>"
$env:COGITO_MODEL_PROVIDER = "openai"
$env:COGITO_MODEL = "gpt-4o-mini" # replace with any model ID available to your API account
cargo run -p harness-cli -- run "Summarize the repository"
```

```bash
# bash
export OPENAI_API_KEY="<your-key>"
export COGITO_MODEL_PROVIDER="openai"
export COGITO_MODEL="gpt-4o-mini" # replace with any model ID available to your API account
cargo run -p harness-cli -- run "Summarize the repository"
```

To use Anthropic, set `ANTHROPIC_API_KEY` and select a Claude model ID. This
example uses a model with adaptive thinking support; model IDs remain freely
configurable:

```powershell
$env:ANTHROPIC_API_KEY = "<your-key>"
$env:COGITO_MODEL_PROVIDER = "anthropic"
$env:COGITO_MODEL = "claude-sonnet-4-6"
cargo run -p harness-cli -- run "Summarize the repository"
```

```bash
export ANTHROPIC_API_KEY="<your-key>"
export COGITO_MODEL_PROVIDER="anthropic"
export COGITO_MODEL="claude-sonnet-4-6"
cargo run -p harness-cli -- run "Summarize the repository"
```

To use Google Gemini, create a key in [Google AI Studio](https://aistudio.google.com/app/apikey),
set `GEMINI_API_KEY`, and choose a model ID available to your account. Harness
uses Gemini's native Generate Content API, including its streaming and function
calling formats:

```powershell
$env:GEMINI_API_KEY = "<your-key>"
$env:COGITO_MODEL_PROVIDER = "gemini"
$env:COGITO_MODEL = "gemini-3.8-flash"
cargo run -p harness-cli -- run "Summarize the repository"
```

```bash
export GEMINI_API_KEY="<your-key>"
export COGITO_MODEL_PROVIDER="gemini"
export COGITO_MODEL="gemini-3.8-flash"
cargo run -p harness-cli -- run "Summarize the repository"
```

OpenCode Zen and Go share the `OPENCODE_API_KEY` credential. Zen uses your Zen
account; Go requires a Go subscription on that account. The available model
catalog is fetched from the selected service and joined with OpenCode's model
metadata. Pick a model using the returned `opencode-zen/<model-id>` or
`opencode-go/<model-id>` name. The provider routes each model to its catalogued
Responses, Chat Completions, or Messages transport.

```powershell
$env:OPENCODE_API_KEY = "<your-key>"
$env:COGITO_MODEL_PROVIDER = "opencode-zen"
$env:COGITO_MODEL = "opencode-zen/gpt-5.6-sol"
cargo run -p harness-cli -- run "Summarize the repository"
```

```bash
export OPENCODE_API_KEY="<your-key>"
export COGITO_MODEL_PROVIDER="opencode-go"
export COGITO_MODEL="opencode-go/glm-5.3"
cargo run -p harness-cli -- run "Summarize the repository"
```

The default API roots are `https://opencode.ai/zen/v1` for Zen and
`https://opencode.ai/zen/go/v1` for Go. Override `COGITO_MODEL_BASE_URL` only
when using a compatible gateway. Both catalogs are cached in memory for 30
minutes; `OpenCodeProvider::refresh_models()` bypasses the cache.

### Connect and manage credentials

The process environment takes priority over the OS credential manager. To
connect interactively from a terminal, run:

```bash
cargo run -p harness-cli -- auth connect
```

Choose a provider when prompted, then enter its key at the hidden prompt. Harness
validates the key with a small model-list request before saving it to Windows
Credential Manager, macOS Keychain, or the Linux Secret Service. You can select a
provider directly, for example `harness auth connect openai`. For scripts and
headless machines, set the provider's environment variable instead; environment
keys remain environment-managed and are never copied into the OS store.

List connection state or delete a key stored by Harness:

```bash
cargo run -p harness-cli -- auth list
cargo run -p harness-cli -- auth disconnect openai
```

`auth list` reports only connected/not connected and whether the value is
environment-managed or stored in the OS credential manager. Disconnect removes
the Harness keychain entry; it does not change environment variables.

In the desktop app, open **Settings → Models → Provider connections**. Entered
keys are sent once to the local runtime, validated there, and stored in the OS
credential manager. The desktop retains only connection status. Environment
credentials are shown as environment-managed and take priority over stored keys.
The runtime provides `credentials.list`, `credentials.validate`,
`credentials.connect`, and `credentials.disconnect` RPC operations; there is no
RPC method for reading a stored secret.

Environment variables are an alternative to connecting in the desktop settings;
they are useful for headless launches and always take precedence over an
OS-stored key. Set the values before starting the CLI or desktop application. The
desktop starts its local runtime automatically after you select a workspace on
first launch, and reconnects to the saved workspace on later launches.

```powershell
$env:ANTHROPIC_API_KEY = "<your-key>"
$env:COGITO_MODEL_PROVIDER = "anthropic"
$env:COGITO_MODEL = "claude-sonnet-4-6"
cargo run -p harness-cli -- run "Summarize the repository"
```

For Gemini, set its runtime environment and run the CLI normally:

```powershell
$env:GEMINI_API_KEY = "<your-key>"
$env:COGITO_MODEL_PROVIDER = "gemini"
$env:COGITO_MODEL = "gemini-3.8-flash"
cargo run -p harness-cli -- run "Summarize the repository"
```

For OpenCode Zen or Go, set the shared key and the corresponding provider and
namespaced model before running the CLI:

```powershell
$env:OPENCODE_API_KEY = "<your-key>"
$env:COGITO_MODEL_PROVIDER = "opencode-zen" # use opencode-go for a Go subscription
$env:COGITO_MODEL = "opencode-zen/gpt-5.6-sol" # choose an ID from the model picker
cargo run -p harness-cli -- run "Summarize the repository"
```

For desktop use, start the app with the environment configured and select a
workspace on first launch. The desktop discovers or starts the RPC runtime and
loads projects and sessions automatically. API keys stay in the local runtime
process and are never sent to the frontend.

| Variable | Purpose | Default |
| --- | --- | --- |
| `COGITO_MODEL_PROVIDER` | `mock`, `openai`, `anthropic`, `gemini`, `opencode-zen`, or `opencode-go` | `mock` |
| `COGITO_MODEL` | Model name passed to the provider | provider default |
| `COGITO_MODEL_API_KEY_ENV` | Name of the variable holding the key | Provider-specific; both OpenCode services use `OPENCODE_API_KEY` |
| `COGITO_MODEL_BASE_URL` | Provider API root; OpenCode defaults differ for Zen and Go, discovery uses `/models` | Provider API root |
| `COGITO_MOCK_REPAIR` | Mock-only test hook; see [Testing the repair loop](#testing-the-repair-loop) | unset |
| `COGITO_RPC_ADDRESS` | Optional fixed loopback endpoint used when `--rpc-address` is not supplied | automatic per-user discovery; OS-assigned loopback port when starting |
| `COGITO_RUNTIME_BINARY` | Optional absolute path override for the managed runtime executable | searched beside the client and in application resources |
| `RUST_LOG` | Runtime log level for the RPC runtime | `info` |

The key is read from the environment only. It is never returned by the settings
API, never placed in an RPC frame, and never logged; clients are told only whether
a credential is present and which variable holds it. The harness stores no key on
disk and implements no home-grown encryption.

The OpenAI adapter uses the Responses API for streaming and function calls. The
Anthropic adapter uses the Messages API, native tool-use/result blocks, and its
SSE event stream. The Gemini adapter uses the native Generate Content API,
function declarations and responses, and SSE streaming. Its paginated
`models.list` discovery results are cached for 15 minutes; the provider exposes
`refresh_models()` to bypass that cache. Listing results are filtered to models
that support `generateContent`, and explicit model IDs remain usable if discovery
is unavailable or incomplete. Model-specific thinking-level and token-budget
rules are validated before sending requests. Gemini thought signatures required
for function-call continuation remain private to the provider adapter and never
enter canonical events or session records. `COGITO_MODEL` is sent as configured,
including model IDs that are not in the local discovery catalog. OpenAI and
Anthropic discovery use local capability metadata because their listing
endpoints do not describe coding suitability or all runtime features. Gemini
discovery includes models that report `generateContent`; the adapter derives
only known capabilities where the listing omits them. A custom
`COGITO_MODEL_BASE_URL` must implement the selected provider's API shape; model
discovery additionally requires its `/models` listing endpoint. Anthropic uses
the official `2023-06-01` API version header. Its adaptive versus manual
thinking parameters and supported effort levels are selected from model
capability metadata; signed thinking blocks needed for tool continuation remain
inside the provider adapter.

OpenCode Zen and Go use the account's authenticated `/models` endpoint for
available IDs and the OpenCode-maintained Models.dev catalog for protocol and
capability metadata. The gateway routes each model to the shared Responses or
Messages adapters, or to its OpenAI-compatible Chat Completions transport. It
does not infer protocols from model names, and models without one of these
catalogued protocols are omitted. Both user-facing providers use the same
`OPENCODE_API_KEY`; Go access depends on the account's Go subscription.

The runtime combines provider catalogs in a unified model registry. Model
identity is provider-qualified (for example, `openai/gpt-4.1-mini` and
`opencode-go/glm-5.3`), so identical model IDs from different providers do not
collide. Successful metadata is cached in `.cogito/model-catalog.json` for 24
hours; the last successful catalog remains available offline after expiration
or a failed refresh. Capability metadata distinguishes supported, unsupported,
and unknown; facts absent from provider metadata remain unknown. No pricing is
shown unless a provider supplies trustworthy pricing metadata. Configured model
IDs remain valid when missing from or stale in the catalog.

The model registry is the source for both clients. The desktop composer picker
groups its discovered models by provider, filters across names and IDs, shows
only capability facts the registry knows, and refreshes catalogs through the
runtime. Missing credentials are shown per provider; **Connect provider** opens
the secure connection form. A reasoning selector appears only when the selected
model advertises supported effort levels.

For scripts and terminals, `harness models` lists the cached registry and
`harness models --refresh` refreshes every provider. Refresh one provider with
`harness models --refresh --provider openai`. `harness model` prints the current
selection, while `harness model provider/model-id` selects a model. Add
`--effort <advertised-level>` only when the model's catalog metadata advertises
that level; `--effort off` disables configurable reasoning. Add `--project` to save
the selection under `[model]` in `.agent/config.toml` instead of as the user
default. A manually entered model ID remains selectable even when it is absent
from discovery.

The CLI stores the user default in the OS user config directory (`models.toml`;
`COGITO_CONFIG_DIR` can choose a portable location). The project setting takes
precedence over that default, and a `model.changed` session event restores a
session-specific choice when the session resumes. Explicit `COGITO_MODEL_*`
environment settings and command-line model flags take precedence over saved
preferences. Model changes made in an active desktop or interactive CLI session
are recorded in that session's event history.

In the interactive CLI, `/models` lists the registry, `/models refresh` refreshes
all providers, `/models refresh openai` refreshes one provider, and
`/model provider/model-id [--effort level]` changes the selection. `/connect`
starts the secure credential flow. Equivalent shell commands include
`harness models`, `harness model openai/model-id`, and `harness auth connect openai`.
Desktop clients use the `models.list` and `models.refresh` RPC methods; provider
API requests and credentials stay in the runtime process.

### Troubleshooting

- If a provider appears disconnected, check the environment of the process that
  launches Harness. Environment credentials take priority over OS-stored keys;
  desktop environment keys must be available to the local runtime, not only to
  the desktop shell.
- If discovery is unavailable, cached catalogs remain usable offline and a
  manually entered model ID can still be selected. Refresh with `harness models
  --refresh` or the desktop model picker's refresh action when network access is
  restored.
- A model absent from OpenCode's picker may not be available to the current
  account or may lack supported protocol metadata. Check the Zen versus Go
  subscription and refresh the matching provider catalog.
- Authentication failures require a valid provider key. Rate limits are
  normalized and retried when safe; repeated 429 responses require waiting for
  the provider's limit window to reset.
- When testing a custom `COGITO_MODEL_BASE_URL`, confirm it implements the
  selected API protocol and, for discovery, its expected model-listing endpoint.

### Adding another provider

Implement a `ModelProvider` adapter in `crates/harness-models`, or add a reusable
wire adapter behind the existing private `ProtocolAdapter` boundary. Translate
requests, streamed events, usage, tool calls, tool results, cancellation, and
normalized errors into the canonical model types. Keep provider-native payloads
and continuation state inside the adapter; do not add provider checks to the
agent loop, session events, or desktop activity renderer. Record only metadata
the catalog or provider can establish, leaving unavailable capabilities
`unknown`.

Add the new route to
[`provider_conformance.rs`](crates/harness-agent/tests/provider_conformance.rs)
so the same deterministic coding-agent fixture runs through it. Add protocol
mock tests for discovery, streaming, tool-result continuation, multiple calls,
usage, errors, cancellation, and secret redaction as applicable. A provider is
not considered supported until its complete tool-use fixture and adapter tests
pass. Then run `cargo test --workspace`, `cargo fmt --all -- --check`,
`cargo clippy --workspace --all-targets -- -D warnings`, and the frontend checks
listed under [Running tests](#running-tests).

## Current v0 capabilities

Everything in this list is implemented and covered by tests.

**Agent and tools**

- Single-agent loop with tool calling, streaming deltas, and usage accounting.
- Runtime-owned goals and revisioned milestone plans persisted in session events;
  compaction, interruption, and session resume preserve objective and progress.
- Default safeguards support up to 256 model turns, 512 tool calls, one hour of
  runtime, and one million reported model tokens; environment limits can lower or
  raise these bounds. Plan revisions remain limited to three per task.
- Fifteen bounded tools: file and process operations plus repository intelligence
  (`search_files`, `search_text`, `find_symbol`, `find_references`,
  `goto_definition`, `get_diagnostics`, `get_file_outline`, and `get_repo_tree`).
- A lazy per-workspace repository index records source languages, declarations,
  imports/exports, package ownership, test files, and configuration files. It is
  queried incrementally; the full index is never added to model context. Text
  searches use ripgrep when installed and a bounded local scan otherwise.
- Initial task context includes a compact root/package overview alongside
  project instructions and Git state. The index refreshes changed files after
  harness edits. Symbol extraction is deterministic and lightweight; it is not
  a compiler or a substitute for a language server.
- The startup repo map is capped at 4 KB and contains only top-level entries,
  package names, selected exported symbols, and concise import relationships.
  Rust, Python, TypeScript/JavaScript, and Go declarations have dedicated
  extractors; other recognized languages retain file/language metadata and
  conservative common declarations.
- `get_diagnostics` starts an on-demand language server when a supported server
  executable is available (`rust-analyzer`, `typescript-language-server`, or
  `pyright-langserver`). If none is installed, it reports that clearly.
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

- A runtime-owned `VerificationPlanner` reads discovered project commands,
  `.agent/config.toml`, and explicit commands in `AGENTS.md` verification sections.
- After an edit it uses available quick diagnostics, then checks the changed
  package or matching test when it can infer one, followed by typecheck, lint,
  build, and a broader test fallback only when a targeted command is unavailable.
  It stops at the first failure so the agent can diagnose and repair before
  spending time on later checks.
- Failures are persisted with their command, category, exit code, relevant
  diagnostics and bounded output, affected files when identifiable, and a
  best-effort introduced/unrelated/unknown attribution. The agent may close an
  unrelated failure only by citing evidence and naming its exact command.
- A Git workspace's final diff is passed to the model after the last edit before
  completion. A task is not marked done while a patch-related verification error
  remains unresolved.
- Goal and plan progress is visible as a compact line in the desktop session and
  inspectable with CLI `/goal` and `/plan-status` (optionally followed by a session
  ID in the line-oriented fallback).
- Git-aware checkpoints and precise, conflict-refusing undo.
- Compaction of long sessions with the full history preserved.

Persistent runtime limits default to 256 turns, 512 tool calls, 3600 seconds, and
1,000,000 model tokens; tune them with `COGITO_AGENT_MAX_TURNS`,
`COGITO_AGENT_MAX_TOOL_CALLS`, `COGITO_AGENT_MAX_RUNTIME_SECONDS`,
`COGITO_AGENT_MAX_MODEL_TOKENS`, `COGITO_AGENT_MAX_COST_USD`,
`COGITO_AGENT_MAX_REPEATED_TOOL_CALLS`, and `COGITO_AGENT_MAX_REPEATED_FAILURES`.
Cost is only estimated or enforced when usage and provider pricing metadata are
both available; otherwise it remains unknown.

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

Before running it from a source checkout, build the workspace once so the
runtime executable is available beside the CLI:

```bash
cargo build --workspace
```

Inspect a workspace without running any project code:

```bash
cargo run -p harness-cli -- inspect .
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

Session and inspection commands:

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
`.cogito/sessions`), `--model-provider`, `--model`, `--rpc-address`, `--json`,
`--yes`, `--log-level`, `--compaction-threshold`. Agent runs are interruptible with
`Ctrl+C`; the cancellation token is shared with tool execution and verification
commands.

The harness RPC runtime starts and connects automatically. Users normally do not
need to start the RPC server manually. In a terminal, `harness` and `harness .` open the interactive interface,
which connects before showing the prompt. `harness run "..."` does the same
bootstrap for a one-shot task. The runtime binary is located beside the CLI
executable or in application resource directories, independently of the current
working directory. Set `COGITO_RUNTIME_BINARY` only when you need to override
discovery.
Startup and connection diagnostics stay off `--json` stdout. Sessions remain
available after either client closes because the shared per-user runtime stays
running.

### Runtime troubleshooting and development

Use these only to inspect or control the persistent runtime while developing or
troubleshooting:

```bash
cargo run -p harness-cli -- runtime status
cargo run -p harness-cli -- runtime restart
cargo run -p harness-cli -- runtime stop
```

If a protocol mismatch is reported, update the CLI and desktop app to matching
releases. A runtime from an incompatible release cannot be shut down over the
incompatible RPC protocol; close that older process from the operating system's
process manager, then retry. Use `--log-level debug` for endpoint, PID,
startup/reuse, protocol, runtime version, and retry diagnostics. Set
`--rpc-address` or `COGITO_RPC_ADDRESS` only when intentionally using a fixed
loopback endpoint. `--workspace <path>` selects the workspace runtime.

`--yes` auto-approves policy prompts. Use it only in a disposable workspace or in
CI.

### Interactive terminal

Start the full-screen terminal interface in a terminal:

```bash
cargo run -p harness-cli
# or explicitly select the current workspace
cargo run -p harness-cli -- .
# the explicit command remains available
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

Task behavior is a separate composer control from execution permission. Choose
`Explore` to ask read/search questions, `Plan` to produce a structured plan
without edits or commands, or `Code` for the full edit-and-verify loop. These
choices never raise the workspace permission mode: for example, CODE with the
execution mode set to Safe still requires policy approval for guarded actions.
The runtime enforces EXPLORE and PLAN tool restrictions, and records the active
mode in the session. Resuming a PLAN session in CODE carries its saved plan and
repository discoveries forward.

The interactive slash commands are `/help`, `/run`, `/resume`, `/inspect`,
`/sessions`, `/status`, `/diff`, `/undo`, `/config`, `/model`, `/models`, `/connect`, `/mode`,
`/explore`, `/plan`, `/code`, `/goal`, and `/plan-status`,
`/clear`, `/cancel`, and `/exit`. `/models` shows the cached model registry;
`/models refresh [provider-id]` refreshes all catalogs or one provider. `/model`
shows the current provider/model; `/model provider/model-id` changes it. `/mode`
reports the execution mode loaded from the
workspace policy; change that setting in `.agent/config.toml` and restart the CLI.
`/explore`, `/plan`, and `/code` select task behavior for the next task. They do
not change `/mode`, which remains the workspace's execution permission level.
After reviewing a PLAN result, use `/code` and submit an implementation request
in the same session to continue from the persisted plan.
`/goal` shows the saved objective and completion condition; `/plan-status` shows
milestone/task progress and plan revisions for the active session. Supply a
session ID to either command when using the line-oriented fallback.
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

The TUI runs one foreground task at a time. It does not keep tasks running in the
background after the interface exits. Use the existing `run`, `resume`, and
`--json` commands for scripts and automation.

If the RPC runtime exits during an active task, the CLI reconnects and refreshes
the saved session state without resubmitting the task. It reports the preserved
session ID and leaves continuation to an explicit `/resume` or `harness resume`
command, so a potentially mutating task is never replayed automatically.

## Running the desktop application

Prerequisites are Rust 1.78+, Node.js 20+, pnpm 10+, and the Tauri 2 platform
dependencies: WebView2 on Windows, Xcode command-line tools on macOS, or
WebKitGTK development packages on Linux.

Start the desktop shell from `apps/desktop`:

```bash
cd apps/desktop
pnpm install
pnpm tauri dev
```

The harness RPC runtime starts and connects automatically. Users normally do not
need to start the RPC server manually.

`pnpm tauri dev` builds the local runtime executable, starts the Vite dev server,
and compiles the Rust shell. On first launch, choose a workspace folder. The
desktop then discovers a healthy runtime or starts it automatically and loads
projects and sessions. Later launches reconnect to the saved workspace and
selected session. The default `auto` endpoint discovers the runtime through the
same per-user metadata as the CLI. Packaged builds include the runtime as a
sidecar. If startup fails, use Retry, Restart runtime, or Open logs in the
recovery strip; endpoint and transport diagnostics appear only when Details is
opened.
Enter a fixed address such as `127.0.0.1:4545` to connect to a manually managed
endpoint. The server is tied to its startup workspace; select a workspace
inside that root or choose another endpoint for a different runtime. RPC has no
authentication, so keep it on the local machine.

The desktop command palette opens with `Ctrl+K` or `Ctrl+Shift+P` (use `Cmd` on
macOS). It can create a task, open a project, resume a session, switch model or
execution mode, show diffs or checkpoints, open the terminal, undo the latest
checkpointed harness change, and open settings. `Ctrl/Cmd+N` starts a task,
`Ctrl/Cmd+P` focuses project and session search, `Ctrl/Cmd+Enter` submits the
composer, and `Ctrl/Cmd+\`` toggles the terminal. `Escape` closes open overlays.

To build the desktop application and bundle its runtime sidecar:

```bash
cd apps/desktop
pnpm tauri build --debug
```

The packaged application includes a target-specific
`cogito-harness-runtime` sidecar. The shared Rust launcher searches the Tauri
resource directory and executable-relative build locations using native paths,
so runtime discovery does not depend on the user's working directory and keeps
spaces and non-ASCII path characters intact. The interactive terminal requires a real terminal emulator to answer the console
host's cursor-position query, which xterm.js does; the CLI's non-interactive shell
tool does not.

See [`apps/desktop/README.md`](apps/desktop/README.md) for the desktop-specific
behaviour, including the human-versus-agent terminal boundary.

## Running tests

Rust, from the repository root:

```bash
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

To measure repository-index startup, search, and single-file update costs against
a synthetic 10,002-file monorepo fixture:

```bash
cargo bench -p harness-tools --bench repository_index
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

`AGENTS.md` can provide repository-specific checks in a `## Verification`,
`## Validation`, or `## Testing` section. Use one command per labeled line; for a
targeted test command, `{changed_files}` expands to the current changed paths:

```markdown
## Verification
- lint: `pnpm lint`
- targeted-test: `pnpm test -- --run {changed_files}`
```

Commands explicitly set in `.agent/config.toml` take precedence for their
category. The planner targets common Cargo package, Go package, pytest, Vitest,
and Jest checks when it can identify a relevant scope; otherwise it retains the
configured test command as a broader fallback.

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

- **No Jev decision layer.** There is no separate decision or adjudication
  service. Planning, repair, and completion checks happen inside the single
  provider-neutral agent loop, with runtime safeguards around repeated calls,
  failures, and resource limits.
- **No subagents.** There is no multi-agent orchestration, delegation, or
  parallelism. `harness-agent` runs exactly one agent per task.
- **No MCP.** There is no Model Context Protocol client or server, and no external
  tool server integration. Tools are compiled-in Rust implementations.
- **No plugin system.** There is no runtime tool or provider discovery, and no
  third-party extension loading. Providers and tools are registered in Rust at
  build time. The `@tauri-apps/plugin-dialog` dependency is Tauri's own native
  file dialog, not a CogitoAI extension point.
- **No remote execution.** The runtime runs as a local process on the machine.
  There is no container sandbox, no SSH or remote host execution, and no
  distributed scheduling.
- **No authentication.** The RPC protocol is unauthenticated and grants full
  control of the opened workspace. It binds to loopback and must not be exposed to
  a network.
- **Configured policy rules are CLI-only in v0.** The CLI loads mode and rules
  from `.agent/config.toml` and enforces both. The desktop enforces the execution
  mode but lists rules without loading them into the engine that authorises tool
  calls. See [Policy rule enforcement](#policy-rule-enforcement).
- **Verification is command-based.** The planner uses deterministic manifest,
  package, test-file, and repository-instruction signals. It does not perform
  semantic test selection, and failure attribution is a best-effort hint that the
  agent must check against the reported output.

## License

MIT. See [`LICENSE`](LICENSE).
