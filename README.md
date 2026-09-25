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
```

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
