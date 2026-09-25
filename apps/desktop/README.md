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
and a chronological session timeline. Approval requests are explicitly
allow-once or deny-once; no permanent policy change is inferred by the UI.
Select a persisted session and use Resume to rehydrate its durable events and
conversation, then continue sending work. Closing the window does not corrupt
the runtime-owned session.

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
