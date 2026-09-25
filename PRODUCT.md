# Product

<!-- impeccable:product-schema 1 -->

## Platform

adaptive

## Stack

Tauri 2, React, TypeScript, Vite, Tailwind, Zustand. The desktop is a presentation and client shell over the Rust harness runtime; it must not duplicate agent logic.

## Users

Solo software developers running CogitoAI against local repositories on their own development machines. They need to select a workspace, observe runtime/session state, converse with the agent, respond to approvals, and inspect the consequences of a run without losing the terminal session when the window closes and reopens.

## Product Purpose

CogitoAI provides a model-agnostic coding-agent runtime with durable sessions, policy-governed tools, verification, checkpoints, and local RPC. The desktop application makes that engine operable and observable from a focused cross-platform workspace.

## Positioning

The desktop is a thin native client to one local runtime, not another agent implementation. Every execution decision, persisted event, approval, cancellation, verification result, and checkpoint mutation remains owned by the Rust runtime and its JSONL session history.

## Operating Context

Developers keep the application open beside an editor and terminal. They open a local repository, choose or resume a session, send tasks, approve or deny tools, watch events, inspect diffs and verification, and close the window without corrupting the active session. Reopening the app reconnects to the runtime and reloads durable state.

## Capabilities and Constraints

- Connect to an already-running loopback JSONL RPC runtime.
- Select a workspace and list/inspect/resume sessions.
- Send user messages and stream typed runtime events.
- Approve or deny pending tool calls and cancel active runs.
- Show Git status/diff, verification context, and safe runtime status.
- Handle runtime unavailable, disconnected runtime, malformed events, workspace loading, and runtime errors.
- Keep the session durable across desktop open/close cycles.
- Tauri commands may expose typed client plumbing but must not contain agent logic.
- v0 is a shell; visual polish, onboarding depth, and secondary workflows are intentionally limited.

## Brand Commitments

CogitoAI is a developer tool: precise, calm, inspectable, and honest about runtime state. The first surface uses a restrained dark operating-tool visual language without decorative chrome.

## Evidence on Hand

The Rust runtime and versioned RPC protocol are implemented in `crates/harness-rpc`. Session, event, policy, checkpoint, verification, and CLI behavior are covered by the existing Rust test suite. No shipped desktop screenshots, brand assets, testimonials, or external product claims exist.

## Product Principles

- One engine, many frontends: never fork runtime behavior into React or Tauri.
- Durable state beats window state: sessions survive the desktop process.
- Truth over reassurance: unavailable, disconnected, malformed, and failed states are first-class.
- Progressive capability: the shell stays useful when the runtime is not available.
- Operator legibility: every agent action has an inspectable runtime event.

## Accessibility & Inclusion

Targets WCAG AA contrast, keyboard-visible focus, semantic controls, and reduced-motion behavior. Status must not rely on color alone.
