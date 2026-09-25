# CogitoAI

CogitoAI is a model-agnostic coding-agent harness. The repository is currently
at the architecture-first stage: it defines the Rust workspace, runtime
contracts, dependency boundaries, and conventions before implementing
provider-specific behavior or privileged tools.

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

Run the current CLI scaffold:

```text
cargo run -p harness-cli -- --help
cargo run -p harness-cli -- --workspace .
```

The CLI currently validates workspace configuration and initializes logging. It
does not yet expose a completed agent runtime.

The desktop application is not scaffolded or runnable yet. Its future setup and
run commands will be added here when the Tauri application exists.

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
