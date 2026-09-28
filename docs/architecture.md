# CogitoAI architecture

For the crate map and step-by-step extension guides, see
[development.md](development.md). For what the project is and how to run it, see
the [README](../README.md).

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
                    +-----------------+------------------+
                    |                                    |
            +-------+--------+                  +--------+--------+
            | harness-rpc    |                  | harness-cli      |
            | runtime + RPC  |                  | CLI client       |
            +-------+--------+                  +-----------------+
                    ^
                    |
          +---------+---------+
          | cogitoai-desktop  |
          | Tauri shell       |
          +-------------------+
```

The manifest graph is slightly wider than this conceptual picture, and
`harness-context` and `harness-verification` both reach down to `harness-tools` and
`harness-git` for the types they assemble. The direction is what matters: nothing
below the line depends on anything above it.

`harness-rpc` is the composition root for capability crates and owns the versioned
loopback JSON-lines transport. `harness-cli` is a presentation client that composes
the same runtime services in-process for its single-process commands. The Tauri
shell links only `harness-rpc`, so it cannot reach a tool, provider, or policy
implementation except through the runtime boundary.

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
| `harness-rpc` | Runtime composition and the client-facing runtime boundary | UI code or direct UI access to privileged implementations |
| `harness-cli` | Command-line parsing and client presentation | Direct filesystem, shell, Git, or provider implementations |

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
cover text, single and multiple tool calls, and a mid-stream failure. OpenCode Zen
and Go are not implemented yet; future catalog entries can choose Responses,
Chat Completions, or Messages adapters individually.

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
RPC connection plumbing; they do not implement agent logic. The shell connects
to a running loopback runtime, and session durability remains in the Rust
JSONL store.

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
