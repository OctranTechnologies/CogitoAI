# CogitoAI architecture

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
             +--------------------+--------------------+
             |                    |                    |
     +-------+------+     +-------+------+     +-------+------+
     | harness-     |     | harness-     |     | harness-      |
     | models       |     | tools        |     | policy        |
     +--------------+     +-------+------+     +---------------+
             ^                    ^                    ^
             |                    |                    |
             +--------------------+--------------------+
                                  |
                         +--------+---------+
                         | harness-rpc     |
                         | composition root |
                         +-----------------+
                                  ^
               +------------------+------------------+
               |                  |                  |
       +-------+------+   +-------+------+   +-------+------+
       | harness-     |   | harness-     |   | harness-      |
       | session      |   | git          |   | verification  |
       +--------------+   +--------------+   +---------------+
                                  ^
                           +------+------+
                           | harness-cli  |
                           | client shell |
                           +-------------+
```

The actual manifest graph is intentionally narrower than the conceptual
picture: `harness-core` has no capability dependencies, and `harness-rpc` is
the composition root for capability crates. `harness-cli` is a presentation
client and composes the same runtime services for its single-process commands;
`harness-rpc` also owns the versioned loopback JSON-lines transport used by
desktop and integration clients.

## Crate responsibilities

| Crate | Owns | Must not own |
| --- | --- | --- |
| `harness-core` | Provider-neutral orchestration traits, shared errors, IDs, configuration, logging setup | Provider APIs, filesystem or shell execution |
| `harness-models` | Model-provider adapters and provider capabilities | Agent lifecycle, tool execution policy |
| `harness-tools` | Tool contracts, tool requests/results, registry | Authorization decisions, provider logic |
| `harness-policy` | Permissions, decisions, and policy enforcement | Tool implementations, UI concerns |
| `harness-session` | Sessions, events, and persistence interfaces | Git operations, provider adapters |
| `harness-git` | Git/checkpoint contracts | General session state |
| `harness-verification` | Test, lint, and typecheck verification contracts | Agent orchestration |
| `harness-rpc` | Runtime composition and the client-facing runtime boundary | UI code or direct UI access to privileged implementations |
| `harness-cli` | Command-line parsing and client presentation | Direct filesystem, shell, Git, or provider implementations |

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
- `harness-models`, `harness-policy`, `harness-session`, `harness-git`, and
  `harness-verification`: depend only on `harness-core`.
- `harness-tools`: depends on `harness-core` and `harness-policy`.
- `harness-rpc`: composes the capability crates.
- `harness-cli`: depends on `harness-core` and external CLI libraries.
