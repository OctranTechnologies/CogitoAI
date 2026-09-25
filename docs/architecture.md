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
the only crate that composes the capability crates. `harness-cli` currently
uses the core configuration contract while the transport/client implementation
is still being designed.

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
verifiers, then exposes operations to external clients. A transport such as
Tauri commands, HTTP, or another RPC protocol should adapt to this boundary;
it should not bypass it.

The desktop client is reserved under `apps/desktop` and is not implemented yet.
When it is added, it will communicate with the Rust runtime through a typed
Tauri command/API layer.

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
