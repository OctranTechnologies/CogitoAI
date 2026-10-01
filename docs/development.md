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
| `harness-tools` | `Tool` trait, `ToolRegistry`, `ToolContext`, `ToolResult`, the seven workspace tools, `LocalProcessRunner`, `CancellationToken` | `harness-core`, `harness-policy`, `harness-session` |
| `harness-git` | `GitClient`, status and diffs, `Checkpoint`/`CheckpointStore` trait, `ShadowCheckpointStore`, runtime-state path exclusion | `harness-core`, `harness-session` |
| `harness-context` | Context assembly from workspace description, instructions, and Git state | `harness-core`, `harness-git`, `harness-session`, `harness-tools` |
| `harness-verification` | `VerificationPlan`, `VerificationStep`, `VerificationReport`, `Verifier` trait, `CommandVerifier` | `harness-core`, `harness-session`, `harness-tools` |
| `harness-pty` | `PtyManager`, human terminal sessions over `portable-pty` | `harness-core` |
| `harness-agent` | `AgentRunner`, `AgentTask`, `AgentOutcome`, `ApprovalHandler`, `AgentLimits`, the tool-calling loop, checkpointing, verification feedback | most of the above |
| `harness-rpc` | `Runtime`, `RpcServer`, `RuntimeConnector`, embedded runtime composition, `ApprovalBroker`, `AgentRunnerFactory`, protocol types, `settings` module, `cogito-rpc-dev` binary | `harness-agent` and the rest |
| `harness-cli` | The `harness-cli` binary: RPC client for agent runs plus focused command helpers | `harness-agent` and the rest |

Non-crate directories:

- `apps/desktop/` — Tauri 2 + React + TypeScript client. `src/lib/rpc.ts` is the
  transport, `src/store.ts` is the Zustand state, `src/lib/events.ts` maps server
  events onto state, and `src-tauri/` holds the Rust shell that bridges Tauri
  commands to the transport.
- `docs/` — this guide and the architecture document.

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
