# CogitoAI desktop design

## Direction

A restrained dark operator console for a solo developer working beside an
editor and terminal. The interface is a thin, legible instrument panel: the
workspace and runtime state are always visible, event history is inspectable,
and action controls are quiet until the agent needs attention.

## Tokens

- Ground: `#0b0d10` with layered `#101318`, `#151920`, and `#1b2029` surfaces.
- Primary text: `#f5f7fa` / `#e7ebf1`; secondary text: `#94a0b2`.
- Action accent: `#38bdf8`, reserved for connection, selection, and primary
  send actions.
- Status: teal `#5eead4` for success, amber `#fbbf24` for approval/attention,
  rose `#fb7185` for failure, with text labels or icons in addition to color.
- Typography: native system sans for interface text and a native monospace
  stack for IDs, paths, commands, and event names.

## Layout

- Top bar: runtime address, workspace selector, connection state, context
  panel toggle.
- Left rail: session list and durable-history affordance.
- Center: workspace identity, event-driven chat stream, composer.
- Right rail: approval queue and live runtime event stream.
- Bottom: runtime status, workspace path, shell/runtime ownership statement.

## Interaction

The shell treats runtime notifications as the source of live state. Assistant
output streams, approvals become actionable cards, and errors remain visible
without obscuring the conversation. Focus rings, reduced-motion behavior, and
disabled/loading states are part of the base system.

## Boundaries

The visual system describes presentation only. Runtime state, tool policy,
session persistence, verification, and checkpoint behavior remain owned by the
Rust runtime and are never inferred or reimplemented in the frontend.
