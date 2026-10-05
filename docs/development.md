# CogitoAI development guide

How the codebase is organised, and how to extend it without breaking the
boundaries the architecture depends on.

For what the project is and how to run it, see the [README](../README.md). For
crate responsibilities and dependency direction, see
[architecture.md](architecture.md).

## Crate map

Crates are named `harness-*`. Dependencies point one way: a crate may depend on
the crates above it in this table and never the reverse.

| Crate | Responsibility | Depends on |
| --- | --- | --- |
| `harness-core` | Shared types (`Error`, `SessionId`, `RunId`, `CheckpointId`, `Timestamp`), workspace discovery, project configuration, instruction loading, logging init | — |
| `harness-models` | Model boundary: `ModelRequest`, `ModelResponse`, content blocks, tool definitions and calls, streaming deltas, `ModelCapabilities`, `ProviderError`, `ModelProvider` trait, mock and OpenAI providers | `harness-core` |
| `harness-policy` | `OperationKind`, `Permission`, `ExecutionMode`, `PolicyRule`, `PolicyEngine`, built-in workspace and credential protections | `harness-core` |
| `harness-session` | Event model, `EventBus`, `SessionStore` trait, `JsonlSessionStore`, compaction records | `harness-core` |
| `harness-tools` | `Tool` trait, `ToolRegistry`, `ToolContext`, `ToolResult`, filesystem/process tools, `RepositoryIndexService` and repository queries | `harness-core`, `harness-policy`, `harness-session` |
| `harness-git` | `GitClient`, status and diffs, `Checkpoint`/`CheckpointStore` trait, `ShadowCheckpointStore`, runtime-state path exclusion | `harness-core`, `harness-session` |
| `harness-context` | Context assembly from workspace description, instructions, and Git state | `harness-core`, `harness-git`, `harness-session`, `harness-tools` |
| `harness-verification` | `VerificationPlan`, `VerificationStep`, `VerificationReport`, `Verifier` trait, `CommandVerifier` | `harness-core`, `harness-session`, `harness-tools` |
| `harness-pty` | `PtyManager`, human terminal sessions over `portable-pty` | `harness-core` |
| `harness-agent` | `AgentRunner`, `AgentTask`, `AgentOutcome`, `ApprovalHandler`, `AgentLimits`, the tool-calling loop, checkpointing, verification feedback | most of the above |
| `harness-rpc` | `Runtime`, `RpcServer`, `RuntimeConnector`, `ProcessRuntimeLauncher`, runtime process composition, `ApprovalBroker`, `AgentRunnerFactory`, protocol types, `settings` module, runtime and `cogito-rpc-dev` binaries | `harness-agent` and the rest |
| `harness-cli` | The `harness-cli` binary: RPC client for agent runs plus focused command helpers | `harness-agent` and the rest |

Non-crate directories:

- `apps/desktop/` — Tauri 2 + React + TypeScript client. `src/lib/rpc.ts` is the
  transport, `src/store.ts` is the Zustand state, `src/lib/events.ts` maps server
  events onto state, and `src-tauri/` holds the Rust shell that bridges Tauri
  commands to the transport.
- `docs/` — this guide and the architecture document.

## Local RPC runtime discovery

`harness-rpc::RuntimeConnector` uses a per-user metadata directory to let the
CLI and desktop reuse one runtime without guessing a port. The runtime binds to
an OS-assigned loopback TCP port because the current RPC client uses
`TcpStream`; the server refuses non-loopback binds. The metadata file is named
`runtime-<workspace-hash>.json` and stores only process/transport/version/endpoint
fields plus a random instance ID. Startup locks use a sibling
`runtime-<workspace-hash>.lock` file.

Automatic startup takes an exclusive OS file lock on that per-workspace lock
file (`flock` on Unix and `LockFileEx` on Windows, through `fs2`). The lock file
is intentionally retained: deleting it while another client has the old file
open can let a third client lock a newly-created file at the same path. The
kernel releases the lock when its handle closes, including after a client
crashes. Windows connector threads are also serialized per lock path because
Windows file locks coordinate processes rather than threads. Once the lock is
acquired, the connector always probes metadata or the explicit endpoint again
before launching. It holds the lock until the new runtime passes readiness, so
other clients wait and then connect to the same instance. Healthy manually
started endpoints are reused by the initial probe and do not trigger a second
launch.

Lock acquisition is bounded by the configured readiness timeout. Timeout
errors include the endpoint and lock path plus a best-effort owner PID. That
diagnostic PID is recorded in a sibling `.lock.owner` file; it is informational
only and may be stale when no lock is held. Neither the PID file nor lock file
contains credentials or other sensitive data.

Default locations:

- Windows: `%LOCALAPPDATA%\\CogitoAI\\runtime`
- macOS: `~/Library/Application Support/CogitoAI/runtime`
- Linux: `$XDG_RUNTIME_DIR/cogitoai`, falling back to
  `$XDG_STATE_HOME/cogitoai/runtime` and then `~/.local/state/cogitoai/runtime`

If the selected application-data directory cannot be created, the connector
falls back to a per-user subdirectory under the system temporary directory.

Discovery checks metadata compatibility, performs a best-effort PID existence
check, then calls `health/check` and compares the live PID and instance ID. Never
use PID existence as proof of identity. Stale cleanup compares the original file
bytes before unlinking so one client cannot remove metadata another client has
just replaced. Readiness uses bounded exponential backoff. Tests should use a
temporary metadata directory via
`RuntimeConnector::with_timing_and_runtime_directory`.

### Runtime process lifecycle

Clients never compose or run the agent loop themselves. The shared
`ProcessRuntimeLauncher` locates `cogito-harness-runtime` beside the CLI binary,
in Tauri resource directories, or at the optional `COGITO_RUNTIME_BINARY`
override. It does not resolve the executable from the caller's working
directory. From a source checkout, build the workspace (`cargo build --workspace`)
to place the development binary under `target/debug` beside `harness`. The
desktop's `beforeDevCommand` builds that target before launching the UI. Tauri
release builds run `apps/desktop/scripts/build-runtime-sidecar.mjs`; the script
creates the target-suffixed file expected by `bundle.externalBin` and packages it
with the application.

The process launcher starts a detached child and sends serialized launch options
through stdin. Provider credentials remain in the inherited environment or OS
credential store and are not placed in the child command line. Child stdout and
stderr are appended to `runtime-<instance-id>.log` in the same per-user runtime
directory as the metadata. After readiness succeeds, the parent releases its
process handle without terminating the child. The runtime remains available when
the CLI or desktop closes. For controlled local shutdown, use
`harness runtime shutdown`; the RPC operation requires the live instance ID.
On Windows, the launcher temporarily prevents inheritance of the client's
standard handles so a persistent runtime cannot keep a parent shell's captured
input or output pipes open.
Healthy runtimes started manually are reused by the same health check and are not
automatically stopped by clients.

## Invariants

These are the rules the rest of the design assumes. Breaking one is a design
change, not a refactor.

1. **Clients hold no privileged logic.** A client may request; the runtime
   decides. Anything with consequences is re-authorised in `harness-rpc` against
   the open workspace, using `harness-policy`, before it happens.
2. **The agent reaches the machine only through the tool registry.** Agent shell
   execution goes through `ToolRegistry`, which applies policy. It must never be
   routed through `harness-pty`.
3. **The workspace is a hard boundary.** Resolve paths with the helpers in
   `harness-tools/src/filesystem.rs` (`resolve_existing` for reads,
   `resolve_for_write` for writes). Do not hand-roll path joining.
4. **Credentials stay in the environment.** Read keys from the environment, pass
   them to a provider, and never place them in a struct that is serialised, a
   response payload, an event, or a log line.
5. **Sessions are append-only.** Add new event variants rather than mutating or
   removing existing ones, and keep `schema_version` handling in mind.
6. **Every privileged operation is event-sourced and checkpointed.** If a tool
   mutates a file, the agent must record it with `record_harness_change`
   immediately, or undo cannot attribute it.

## Adding a model provider

A provider implements the normalized `ModelProvider` trait in
`crates/harness-models/src/lib.rs`:

```rust
pub trait ModelProvider: Send + Sync {
    fn descriptor(&self) -> ModelDescriptor;
    fn discover_models(&self) -> Result<Vec<ModelDescriptor>, ProviderError>;
    fn refresh_models(&self) -> Result<Vec<ModelDescriptor>, ProviderError>;
    fn complete(&self, request: &ModelRequest) -> Result<ModelResponse, ProviderError>;
    fn stream(
        &self,
        request: &ModelRequest,
        on_event: &mut dyn FnMut(ModelStreamEvent) -> Result<(), ProviderError>,
    ) -> Result<ModelResponse, ProviderError>;
}
```

To add one:

1. Implement the trait in `harness-models`. Translate the vendor's wire format
   into canonical responses and normalized events. Keep vendor types private to
   the adapter and report failures as `ProviderError` variants.
2. Add a variant to `ProviderKind` with an explicit Serde name if its public
   configuration ID uses punctuation such as `opencode-zen`.
3. Register the constructor in `provider_from_config`, which both clients call.
4. Add provider defaults to `ModelConfig` when needed. Keep only the API-key
   environment variable name in configuration, never the secret value.
5. Read the key from `api_key_env` inside the provider constructor and keep it
   in the private transport. Redact it from Debug output and errors.
6. Add mock-server coverage for `complete`, streaming, tools, errors, and
   cancellation rather than requiring a live endpoint.

`ScriptedMockProvider` is the reference implementation for deterministic tests.
`OpenAIProvider`, `AnthropicProvider`, `GeminiProvider`, and `OpenCodeProvider`
are the real HTTP adapter references. They share the normalized model contract
and keep Responses, Messages, Generate Content, and Chat Completions wire
protocols private to their transports. OpenCode's Zen and Go catalog entries
choose a protocol through maintained metadata; the gateway filters unsupported
protocol declarations rather than guessing from model names.
Gemini model discovery is paginated and cached for 15 minutes; call
`GeminiProvider::refresh_models()` to bypass the cache.
OpenCode model discovery joins the authenticated account listing with the
maintained metadata catalog, caches for 30 minutes, and also exposes
`OpenCodeProvider::refresh_models()`.

The shared `ModelRegistry` aggregates these adapters by provider-qualified
identity, stores successful catalogs in `.cogito/model-catalog.json`, and keeps
stale catalogs available when discovery fails or the runtime starts offline.
Register additional `ModelProvider` instances with `ModelRegistry::register_provider`
to add future catalog sources without changing RPC or CLI aggregation. Catalog
capability filters match only explicitly supported features; unknown metadata
does not satisfy a required capability. Runtime clients use `models.list` and
`models.refresh`; the interactive CLI exposes `/models` and
`/models refresh [provider-id]`.

## Adding a tool

A tool is anything implementing `Tool` in `crates/harness-tools/src/lib.rs`:

```rust
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    fn required_permission(&self) -> Permission;
    fn operation(&self) -> OperationKind;
    fn execute(
        &self,
        context: &ToolContext<'_>,
        request: ToolRequest,
    ) -> Result<ToolResult, ToolError>;
}
```

To add one:

1. Implement `Tool`. `spec()` is the JSON schema advertised to the model, so
   describe arguments precisely; a vague schema produces a vague tool call.
2. Choose `operation()` and `required_permission()` honestly. `OperationKind`
   drives the mode defaults, so a tool that writes must report `Write` or `Patch`
   or it will run unattended in `read-only`.
3. In `execute`, resolve every path through `resolve_existing` or
   `resolve_for_write` so containment is enforced, and honour
   `context.cancellation` if the tool can run long.
4. Register it in `ToolRegistry::with_workspace_tools_cancellation` in
   `crates/harness-tools/src/lib.rs`, which is the single place the standard set
   is assembled. Note that a new tool is not automatically available over RPC; see
   below.
5. Bound your output. Return a `ToolResult` and set `truncated` rather than
   returning an unbounded payload, and report failures as `ToolError`.
6. Add tests for the happy path, a path outside the workspace, and the largest
   input you expect.

If the tool mutates a file, the agent must call
`CheckpointStore::record_harness_change` after the write, or undo will not be able
to restore it.

## Repository index and search

The standard workspace registry registers the eight repository queries from
`crates/harness-tools/src/repository_index.rs`. Keep their results concise and
bounded; the index itself must not be appended to model context. The same lazy
`RepositoryIndexService` supplies the startup repo map and model-facing tools.
Successful filesystem writes call `update_changed` before returning to the next
model turn. Add fixtures for each language parser or package-manifest shape you
change, and verify that a renamed/removed declaration disappears from the next
query.

The index currently uses deterministic declaration patterns rather than a
Tree-sitter grammar. Keep extraction explicitly approximate, and prefer an LSP
diagnostic query for compiler-level feedback where one of the supported server
programs is available. Text search should continue to use ripgrep with a bounded
native fallback so packaged runtimes remain usable without an `rg` executable.

Run the synthetic 10,002-file performance fixture from the workspace root with:

```bash
cargo bench -p harness-tools --bench repository_index
```

It reports index startup, indexed symbol lookup, ripgrep search, and single-file
incremental update timings. The benchmark is a local performance aid, not a
machine-specific CI threshold.

## Adding a policy rule

Most rules need no code. Add them to `.agent/config.toml` (enforced by the CLI)
or `.agent/policy.toml` (listed by the desktop Permissions screen):

```toml
[[policy.rules]]
name = "deny-generated"
action = "deny"
priority = 10
paths = ["generated/**"]
```

Matching accepts `tools`, `operations`, `modes`, `paths` (globs relative to the
repository root), and `command_patterns`. Resolution order is: an explicit
`deny` match always wins, then the highest `priority` match, then the mode
default.

To change code-level policy:

- To change what a mode does by default, edit `PolicyEngine::default_evaluation`.
  Remember that the workspace-boundary and credential-path checks in
  `evaluate_request` run before any rule and are not overridable.
- To change which rules the desktop reports as built in, update `built_in_rules()`
  in `crates/harness-rpc/src/settings.rs` so the Permissions screen tells the
  truth.
- To add a new `OperationKind`, extend the enum in `harness-policy` and handle it
  in both `default_evaluation` and `OperationKind::permission()`. A new kind that
  is not handled will not compile, which is the intended safeguard.

Add a test for each rule in `crates/harness-policy/tests/policy_engine.rs`,
including the case where a rule of the opposite action also matches.

## Adding a frontend client surface

A new capability should reach both clients from one runtime change. Work outward
from the runtime.

1. **Runtime.** If the capability is a new RPC method, add a handler in
   `crates/harness-rpc/src/server.rs` and register it in the method dispatch
   table. Validate the request, and authorise it server-side against the open
   workspace; never trust a path or command from the client.
2. **Protocol.** If the request or response shape is new, extend the types in
   `crates/harness-rpc/src/protocol.rs`. Keep `RPC_PROTOCOL_VERSION` in mind and
   bump it for a breaking change. Prefer additive changes.
3. **Event.** If clients should react without polling, append a new
   `EventPayload` variant in `crates/harness-session/src/events.rs`, give it an
   `EventType`, and emit it from the runtime. Add the variant rather than
   repurposing an existing one, since stored sessions must stay replayable.
4. **Transport.** In `apps/desktop/src/lib/rpc.ts`, add a typed wrapper over
   `requestRuntime` if the call is request/response. For streaming, handle the
   notification in `handleServerMessage`.
5. **State.** Map the event in `apps/desktop/src/lib/events.ts` and expose an
   action in `apps/desktop/src/store.ts`. The store is the only place that
   mutates client state.
6. **UI.** Add a component under `apps/desktop/src/components/` and wire it into
   `App.tsx`. Keep components presentational; they read the store and call store
   actions. Style it with the tokens in `src/styles.css` and the primitives in
   `src/components/ui/` rather than literal values, and see
   [`apps/desktop/README.md`](../apps/desktop/README.md) for the design system.
7. **Tests.** Add a Vitest case in the matching `*.test.ts` and, if the behaviour
   is runtime-side rather than presentation, a case in
   `crates/harness-rpc/tests/rpc_loopback.rs` that drives it over a real socket.

The same method should also be reachable from `harness-cli` if it is useful
non-interactively, so the two clients do not diverge in capability.

## Styling the desktop

Two rules, both enforced rather than merely preferred.

**One palette.** A literal colour is only allowed in `src/styles.css`. Everything
else uses a semantic token, so `tailwind.config.js` maps `bg-panel` and
`text-muted` onto the custom properties declared there. If a token is missing,
add it to `:root`, to the `[data-theme="light"]` block, and to the `FALLBACK` map
in `src/lib/tokens.ts` so the editor and the terminal see it too.

A useful trap to know about: setting `theme.spacing` at the top level of the
Tailwind config **replaces** the default scale, which silently deletes every
`p-*`, `m-*`, `gap-*`, and `min-h-*` utility while the build still succeeds. Add
named steps through `theme.extend.spacing` instead.

**Primitives over ad-hoc markup.** `src/components/ui` owns the shared
behaviour: dismissal on Escape and outside click, focus restoration, the focus
trap, and the keyboard contract for menus. If a component needs a new floating
surface, extend the primitives rather than writing a fourth dismissal handler.

Run `pnpm check:classes` after styling work. It compares the class names used in
components against the generated CSS, so a class that resolves to nothing is a
failure rather than a quietly unstyled element.

## Testing conventions

- Rust tests live in `crates/<crate>/tests/` and drive real components. Prefer a
  real temporary repository over a mocked `GitClient`.
- A test that starts a process, opens a PTY, or runs `cargo` needs a generous
  deadline, and must wait for evidence that the thing it started is actually
  running before asserting on its output. Interrupting a command before its child
  has attached is a race, not a failure.
- Tests that place a decoy file outside the workspace must own the directory they
  place it in. The system temp directory is shared, and parallel tests will delete
  each other's fixtures.
- Never put a real credential in a test. Use an obviously fake sentinel, and
  assert on its absence when testing redaction.
- Frontend tests are Vitest with jsdom. Keep the transport mocked; cover real
  protocol behaviour in the Rust loopback tests instead.

## Before you push

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cd apps/desktop && pnpm typecheck && pnpm lint && pnpm test && pnpm build
```

The workspace forbids `unsafe_code`, so a change that needs it will not compile.
Clippy runs with `-D warnings` in CI expectations, so fix lints rather than
allowlisting them.
