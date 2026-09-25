# CogitoAI desktop shell

This directory contains the initial Tauri 2 + React + TypeScript desktop
client. It is a presentation layer over the versioned `harness-rpc` loopback
protocol. Tauri commands only manage the RPC transport; agent logic remains in
the Rust runtime.

## Prerequisites

- Rust 1.78+
- Node.js 20+
- pnpm 10+
- Tauri 2 platform prerequisites: WebView2 on Windows, Xcode command-line
  tools on macOS, or WebKitGTK development packages on Linux.

## Development

Start a local development RPC runtime from the repository root:

```text
cargo run -p harness-rpc --bin cogito-rpc-dev -- . 127.0.0.1:4545
```

Then start the desktop shell:

```text
cd apps/desktop
pnpm install
pnpm tauri dev
```

Enter the runtime address and workspace path in the shell, then select
Connect. The conversation panel shows prompts, streamed assistant output, and
run phase. The context panel shows expandable tool cards, verification results,
a checkpoint timeline, and a chronological session timeline. Approval requests
are explicitly allow-once or deny-once; no permanent policy change is inferred
by the UI. Select a persisted session and use Resume to rehydrate its durable
events and conversation, then continue sending work. Closing the window does
not corrupt the runtime-owned session.

## Code changes and checkpoints

The Changes view is a read-only code observability surface built on Monaco
Editor:

- Changed files grouped into added, modified, and deleted, with per-file
  additions and deletions.
- Per-file diffs in Monaco with a split/inline toggle, plus a plain source
  viewer for files without a before/after.
- Syntax highlighting from a locally bundled set of tokenizers. Monaco ships
  from the local bundle (not a CDN) and its language *services* are not loaded,
  so the app works fully offline and the shell stays fast to open.
- A checkpoint timeline listing checkpoint ID, timestamp, the triggering task,
  and affected files, with a Restore control per checkpoint.

Restore calls `checkpoint.undo` on the runtime. All restore safety behavior —
scoping to the files the checkpoint recorded, detecting conflicts, and leaving
unrelated working-tree changes alone — lives in the Rust runtime. The frontend
issues no filesystem writes.

The changes view is driven by runtime events: it refreshes when a run reports a
file change, when a mutating tool completes, and when a run ends or a
checkpoint is restored. Use the refresh control to re-read on demand. The
runtime's own `.cogito/` state is excluded from reported code changes.

## Checks

```text
cd apps/desktop
pnpm typecheck
pnpm lint
pnpm test
pnpm build
pnpm tauri build --debug
```

The v0 transport is loopback-only and unauthenticated. Do not bind it to a
public interface.
