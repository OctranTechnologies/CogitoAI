# CogitoAI architecture

For the crate map and step-by-step extension guides, see
[development.md](development.md). For what the project is and how to run it, see
the [README](../README.md).

## Core coding loop

The product is organized around one software-engineering task loop:

```text
UNDERSTAND
    → PLAN when the task needs it
    → SEARCH / READ
    → EDIT
    → VERIFY
    → INSPECT DIFF
    → REPAIR if verification or review finds a problem
    → FINISH with the result and remaining caveats
```

`harness-agent` coordinates the loop without provider-specific branches. It
builds bounded context with `harness-context`, requests normalized model output
from `harness-models`, and routes tool calls through `harness-tools` and
`harness-policy`. Mutations are recorded by `harness-session` and checkpointed
through `harness-git`; configured checks run through `harness-verification`.
The agent then gets verification results and the current workspace state for a
repair turn when needed. Finishing leaves the user with the final response,
session history, verification results, and inspectable changes. `harness-rpc`
composes these capabilities into the shared local runtime used by both clients.

Planning is a model judgment, not a separate workflow engine. Search, read,
edit, verification, and repair are ordinary normalized model/tool turns under
the same policy and event contracts. The desktop and CLI present the workflow;
they do not implement agent behavior.

## Dependency direction

The workspace separates contracts, capabilities, and composition. Dependencies
point toward the capability being used, never from a capability back to a UI or
provider implementation.

```text
                          +----------------+
                          | harness-core   |
                          | contracts, IDs |
                          +--------+-------+
                                   ^
        +-------------+-------------+-------------+-------------+
        |             |             |             |             |
   +----+-----+  +-----+-----+  +----+-----+  +--+-------+  +--+------+
   | harness- |  | harness- |  | harness- |  | harness- |  | harness- |
   | models   |  | policy   |  | session  |  | git      |  | pty     |
   +----+-----+  +-----+-----+  +----+-----+  +--+-------+  +--+------+
        ^             ^             ^             ^             |
        |             |             |             |             |
        +-------------+------+------+-------------+             |
                            |      |                            |
                   +--------+--+ +-+--------------+             |
                   | harness-  | | harness-      |             |
                   | tools     | | verification  |             |
                   +--------+--+ +-+--------------+             |
                            |      |                            |
                            |  +---+--------------+             |
                            |  | harness-context  |             |
                            |  +------------------+             |
                            |         ^                          |
                            |         |                          |
                            +---------+--------------------------+
                                      |
                             +--------+---------+
                             | harness-agent    |
                             | the agent loop   |
                             +-----------------+
                                      ^
                                      |
            +------------------------+
            | harness-rpc            |
            | runtime + RPC +        |
            | RuntimeConnector       |
            +-----------+------------+
                        ^
              +---------+---------+
              |                   |
      +-------+--------+   +------+-----------+
      | harness-cli    |   | cogitoai-desktop |
      | CLI client     |   | Tauri shell      |
      +----------------+   +------------------+
```

The manifest graph is slightly wider than this conceptual picture, and
`harness-context` and `harness-verification` both reach down to `harness-tools` and
`harness-git` for the types they assemble. The direction is what matters: nothing
below the line depends on anything above it.

`harness-rpc` is the composition root for capability crates and owns the versioned
loopback JSON-lines transport. Its `RuntimeConnector` handles endpoint discovery,
health checks, cross-process startup locking, managed process startup, readiness,
reconnection, and connection state. The CLI routes agent runs through that shared
RPC runtime; its inspection and configuration commands retain their focused CLI
helpers. The Tauri shell links only `harness-rpc`, so it cannot reach a tool,
provider, or policy implementation except through the runtime boundary.

With no explicit endpoint override, the connector reads a per-user metadata
record keyed by a hash of the canonical workspace path. It validates the
metadata format and transport, treats PID liveness only as a hint, then calls
`health/check` and matches the live process ID and instance ID. That exchange
negotiates client/server RPC protocol versions and reports client/runtime
versions before normal RPC methods are used. A healthy incompatible runtime is
preserved and reported with a recovery step; it is never mistaken for stale
metadata or replaced in a retry loop. PID presence alone never establishes
identity. Invalid or stale records are removed only if their bytes have not
changed since they were read. If no matching runtime is healthy,
a per-workspace startup lock serializes the launch. `ProcessRuntimeLauncher`
locates the packaged sidecar or local workspace build independently of the
client's working directory, starts it detached, and sends non-secret startup
configuration over stdin. The child announces the bound endpoint and identity in
a private, atomically written readiness file. The connector writes per-user
metadata and polls health with bounded exponential backoff. The current JSON-lines
client uses `TcpStream`, so transport remains loopback TCP; the RPC server rejects
non-loopback bind addresses. A healthy runtime is reused, and dropping a client
closes its socket but leaves the runtime available to other local clients.

Readiness and client reconnect delays use bounded exponential backoff with
jitter. Each mutation carries a unique request ID that also acts as its default
idempotency key; a caller retrying an ambiguous request can reuse an explicit
key. The runtime keeps a bounded, in-memory result cache for matching mutation
retries and rejects a key reused with a different method or payload. Client
recovery never resubmits an ambiguous in-flight task after the runtime itself has
restarted; it reconnects and refreshes persisted session state instead. Loopback
binds to `127.0.0.1`, so local RPC discovery does not depend on the machine's
external network interface.

The runtime process is intentionally persistent across client lifetimes. It
inherits the launch environment for provider credentials, receives no credential
on its command line, and writes stdout/stderr to a per-instance log next to the
runtime metadata. A successful client disconnect does not stop it. The explicit
`rpc.shutdown` admin call checks the runtime's instance ID; the CLI exposes this
as `harness runtime shutdown` for development and troubleshooting.

Runtime metadata contains only PID, metadata and RPC protocol versions,
transport, endpoint, startup timestamp, runtime version, and instance ID. It
does not contain the workspace path, credentials, or session data. The default
directory is `%LOCALAPPDATA%\\CogitoAI\\runtime` on Windows,
`~/Library/Application Support/CogitoAI/runtime` on macOS, and
`$XDG_RUNTIME_DIR/cogitoai` on Linux when available (otherwise
`$XDG_STATE_HOME/cogitoai/runtime` or `~/.local/state/cogitoai/runtime`). Unix
directories and files are restricted to the current user. A workspace-hash
startup lock sits beside the metadata file. If the platform application-data
directory cannot be created, the connector falls back to a per-user directory
under the system temporary directory.

## Crate responsibilities

| Crate | Owns | Must not own |
| --- | --- | --- |
| `harness-core` | Provider-neutral orchestration traits, shared errors, IDs, configuration, logging setup, workspace discovery | Provider APIs, filesystem or shell execution |
| `harness-models` | Model-provider adapters and provider capabilities | Agent lifecycle, tool execution policy |
| `harness-tools` | Tool contracts, tool requests/results, registry, workspace tools, process execution | Authorization decisions, provider logic |
| `harness-policy` | Permissions, decisions, and policy enforcement | Tool implementations, UI concerns |
| `harness-session` | Sessions, events, and persistence | Git operations, provider adapters |
| `harness-git` | Git/checkpoint contracts and shadow checkpoint storage | General session state |
| `harness-context` | Context assembly from workspace, instructions, and Git state | Agent lifecycle, provider adapters |
| `harness-verification` | Test, lint, and typecheck verification contracts | Agent orchestration |
| `harness-pty` | Human terminal sessions over a native pseudo-terminal | Any `Tool` implementation, so the agent cannot reach it |
| `harness-agent` | The agent loop, approvals, checkpoint recording, verification feedback | Transport, UI concerns |
| `harness-rpc` | Runtime composition, connector lifecycle, and client-facing RPC boundary | UI code or direct UI access to privileged implementations |
| `harness-cli` | Command-line parsing, RPC client presentation, and focused read/config commands | The agent loop and direct shell execution for agent tasks |

## Model provider boundary

`harness-models` owns the canonical model contract: descriptors, capability
flags, requests, responses, tool definitions/calls/results, usage, finish
reasons, reasoning configuration, provider errors, and normalized stream events.
`ModelProvider::generate` checks a request against the descriptor's
capabilities and emits the same event vocabulary whether an adapter streams or
must synthesize events from a complete response. The agent loop handles only
those normalized events and a completed response; it does not inspect provider
names or wire payloads.

Provider facades are responsible for selecting a private protocol adapter for
the configured model. The private `ProtocolAdapter` boundary and shared HTTP
JSON/SSE framing utilities allow one provider facade to route models to
different protocols without leaking JSON payloads into runtime types. The
native OpenAI adapter uses `OpenAIResponsesTransport` and the Responses API
for streaming text, function calls, tool results, usage, and reasoning effort.
The Anthropic adapter uses `AnthropicMessagesTransport` and Messages API content
blocks for streamed text, `tool_use`, `tool_result`, usage, and model-aware
thinking controls. Both transports normalize API errors, retry only transient
failures before stream consumption, and check cancellation while waiting for SSE
data. API keys are held only in memory and are not included in adapter errors or
events. Anthropic thinking signatures required for tool continuation are held in
private in-memory adapter state and replayed without entering canonical events or
session records. OpenAI and Anthropic filter their official `GET /models` results
through local capability metadata because the listing endpoints do not report
tool or coding-agent suitability; explicitly configured model IDs remain usable
even when they are not in that metadata. Anthropic thinking style and effort
controls are selected from model-specific local metadata rather than inferred
from the provider name alone. Gemini uses `GeminiNativeTransport` with the native
Generate Content API, paginated `models.list`, streaming responses, and native
function declaration/result messages. Discovery results are cached per transport
for 15 minutes and `refresh_models` forces a reload. Model IDs remain explicitly
configurable if discovery is unavailable. Optional model-list fields fall back to
known model-family capabilities, while model-specific thinking levels and budgets
are validated against Gemini's documented support ranges. Gemini thought
signatures are held in private adapter memory and restored to the canonical
assistant/tool transcript only when constructing the next native request. They
never enter core, events, session data, or the frontend. Each agent turn sends its
fresh assembled context followed by the latest canonical assistant/tool batch,
so native function-result protocols can continue while older observations remain
subject to context compaction and budget limits. Deterministic mock adapters
cover text, single and multiple tool calls, and a mid-stream failure. OpenCode
Zen and Go are gateway facades, not new agent-loop branches. Each one joins its
authenticated `/models` listing with OpenCode's maintained model metadata
catalog, caches the result locally for 30 minutes, and routes using the model's
declared AI SDK protocol (`@ai-sdk/openai`, `@ai-sdk/openai-compatible`, or
`@ai-sdk/anthropic`). Responses and Messages delegate to the existing adapters;
the shared compatible Chat Completions adapter handles streaming deltas,
parallel calls, tool-result continuation, usage, errors, and cancellation.
Models without one of those protocol declarations are not advertised. Both
services use the same `OPENCODE_API_KEY`; Go additionally requires the account's
Go subscription. An explicit `refresh_models` call bypasses the cache.

`ModelRegistry` combines provider catalogs using the canonical identity
`provider_id/model_id`, so duplicate native IDs remain distinct. Its cache is
workspace-local at `.cogito/model-catalog.json`, expires after 24 hours, and
retains expired successful data for offline startup and failed refreshes.
Catalog provenance is reported as discovered, cached, or manually configured;
capability knowledge is tri-state, and missing provider metadata stays unknown.
The registry retains each provider's configured default and never rejects a
manually configured ID because it is absent or stale in a catalog. Runtime RPC
methods `models.list` and `models.refresh` keep provider HTTP requests out of
desktop clients; the CLI uses the same registry through `/models` and
`/models refresh [provider-id]`.

Capabilities describe what the selected model adapter can substantiate, not
what a provider family might support in general. Unknown limits and features
remain absent/false. Private chain-of-thought is not surfaced as
`reasoning.delta`; adapters may emit that event only for content safe to expose,
such as a provider-supplied summary. `harness-core` remains free of model and
provider types.


## Runtime boundary

`harness-rpc::Runtime` is the composition root. It receives the agent runtime,
model providers, tool registry, policy, session store, checkpoint store, and
verifiers, then exposes operations to external clients. `RpcServer` adapts
those operations to a versioned newline-delimited JSON protocol over loopback
TCP. The server delegates to the same capability crates used by the CLI; it
does not duplicate the model loop, tool dispatch, policy evaluation, context
assembly, checkpoint recording, or session reconstruction.

`RpcClient` is a transport client. Agent runs are asynchronous and return a
`run_id`; the server streams durable `HarnessEvent` values as notifications and
returns approval, cancellation, and terminal notifications separately. v0 uses
one active run per server because concurrent mutations do not yet have
independent event correlation. A dropped client denies pending approvals and
cancels the active run. The transport is loopback-only and has no
authentication or encryption; hosts must not bind it to a public interface.

The desktop shell is under `apps/desktop`. It is implemented with Tauri 2,
React, TypeScript, Vite, Tailwind, and Zustand. Its Tauri commands own only
RPC connection plumbing; they do not implement agent logic. On first use the
shell asks for a workspace, then its Tauri bridge uses the same Rust
`HarnessConnectionManager` as the CLI to discover or start the loopback runtime.
The bridge forwards lifecycle state events to React without exposing endpoint or
error details in normal UI. On transport loss, the shell keeps its local view,
reconnects with bounded retries, and reloads the authoritative workspace/session
state; it never resends the interrupted task. Session durability remains in the
Rust JSONL store. Packaged Tauri builds include the target-specific runtime as a
sidecar, and binary discovery uses resource and executable-relative paths rather
than the user's working directory.

## Privileged operations and UI clients

The desktop process is a client, not a trusted part of the harness runtime.
Users can alter UI code, invoke developer tools, or submit arbitrary command
arguments. Therefore the UI must not directly execute privileged operations:

- filesystem reads or writes;
- shell commands and subprocesses;
- network access;
- Git mutations and checkpoint restoration;
- policy bypasses or credential access.

The UI may request an operation, but the runtime must validate the request and
apply policy before delegating it to `harness-tools`, `harness-git`, or
`harness-verification`. The UI must not receive a shell handle, unrestricted
filesystem handle, provider key, or policy implementation. This keeps the
security boundary in one place and makes audits, future transports, and
headless operation consistent.

## Shared conventions

- Use `harness_core::Error` across public crate boundaries.
- Use `harness_core::Id` and its aliases for opaque identifiers.
- Use `tracing` for instrumentation and `harness_core::init_logging` for process
  initialization.
- Keep configuration explicit in `HarnessConfig`; do not read secrets or choose
  provider defaults inside UI code.
- Keep crates synchronous in this first architecture pass. Async execution and
  transport details should be introduced at the `harness-rpc` boundary without
  leaking into provider or tool contracts unnecessarily.

## Verification

Dependency direction is checked with:

```text
cargo metadata --format-version 1 --no-deps
cargo tree --workspace --edges normal
```

The expected internal graph is:

- `harness-core`: no workspace dependencies.
- `harness-models`, `harness-policy`, `harness-session`, and `harness-pty`: depend
  only on `harness-core`.
- `harness-git`: depends on `harness-core` and `harness-session`.
- `harness-tools`: depends on `harness-core`, `harness-policy`, and
  `harness-session`.
- `harness-verification`: depends on `harness-core`, `harness-session`, and
  `harness-tools`.
- `harness-context`: depends on `harness-core`, `harness-git`, `harness-session`,
  and `harness-tools`.
- `harness-agent`: composes the capability crates and owns the loop.
- `harness-rpc`: composes the capability crates and owns the transport.
- `harness-cli`: depends on `harness-agent` and the capability crates.
- `cogitoai-desktop`: depends on `harness-rpc` only, which is what keeps the
  shell from reaching a privileged implementation directly.
