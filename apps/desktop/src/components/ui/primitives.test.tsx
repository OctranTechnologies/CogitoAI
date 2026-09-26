import { describe, expect, it } from "vitest";
import { renderToString } from "react-dom/server";
import { createElement } from "react";
import {
  Badge,
  Button,
  CommandMenu,
  ContextMenu,
  Dropdown,
  EmptyState,
  IconButton,
  Modal,
  Panel,
  PanelHeader,
  Popover,
  ScrollArea,
  Separator,
  StatusIndicator,
  Tooltip,
} from "./index";

const render = (node: Parameters<typeof renderToString>[0]) => renderToString(node);

describe("ui primitives", () => {
  it("renders a primary button with its accessible name", () => {
    const html = render(createElement(Button, { variant: "primary" }, "Send"));
    expect(html).toContain("Send");
    expect(html).toContain("type=\"button\"");
  });

  it("defaults a bare button to type=button so it cannot submit a form by accident", () => {
    const html = render(createElement(Button, null, "x"));
    expect(html).toContain('type="button"');
  });

  it("gives an icon-only button both an aria-label and a title", () => {
    const html = render(createElement(IconButton, { label: "Refresh changes" }, "x"));
    expect(html).toContain('aria-label="Refresh changes"');
    expect(html).toContain('title="Refresh changes"');
  });

  it("marks a disabled control and keeps it non-interactive", () => {
    const html = render(createElement(Button, { disabled: true }, "Send"));
    expect(html).toContain("disabled");
  });

  it("renders a badge tone through a token class rather than a literal colour", () => {
    const html = render(createElement(Badge, { tone: "error" }, "failed"));
    expect(html).toContain("text-error");
    expect(html).not.toMatch(/#[0-9a-f]{6}/i);
  });

  it("maps a runtime status onto a status indicator tone", () => {
    expect(render(createElement(StatusIndicator, { status: "connected", label: false }))).toContain("text-success");
    expect(render(createElement(StatusIndicator, { status: "error", label: false }))).toContain("text-error");
    expect(render(createElement(StatusIndicator, { status: "running", label: false }))).toContain("text-warning");
  });

  it("exposes a separator to assistive technology", () => {
    const html = render(createElement(Separator, { orientation: "vertical" }));
    expect(html).toContain('role="separator"');
    expect(html).toContain('aria-orientation="vertical"');
  });

  it("renders a tooltip bubble only when open", () => {
    const closed = render(createElement(Tooltip, { label: "Explain", children: createElement("span", null, "?") }));
    expect(closed).not.toContain('role="tooltip"');
    expect(closed).toContain("?");
  });

  it("renders a panel with a header", () => {
    const html = render(
      createElement(Panel, null, createElement(PanelHeader, { title: "Runtime context" })),
    );
    expect(html).toContain("Runtime context");
  });

  it("renders a scroll area and an empty state", () => {
    expect(render(createElement(ScrollArea, null, "body"))).toContain("scroll-area");
    expect(render(createElement(EmptyState, null, "Nothing yet"))).toContain("Nothing yet");
  });

  it("renders nothing for a closed modal and a dialog for an open one", () => {
    expect(render(createElement(Modal, { open: false, onClose: () => {}, title: "Settings" }, "body"))).toBe("");
    const html = render(createElement(Modal, { open: true, onClose: () => {}, title: "Settings" }, "body"));
    expect(html).toContain('role="dialog"');
    expect(html).toContain('aria-modal="true"');
    expect(html).toContain("Settings");
  });

  it("renders the command palette only while open", () => {
    const items = [{ id: "a", label: "New session", onSelect: () => {} }];
    expect(render(createElement(CommandMenu, { open: false, onClose: () => {}, items, label: "Commands" }))).toBe("");
    const html = render(
      createElement(CommandMenu, { open: true, onClose: () => {}, items, label: "Commands" }),
    );
    expect(html).toContain('role="listbox"');
    expect(html).toContain("New session");
  });

  it("keeps a closed popover, dropdown, and context menu out of the markup", () => {
    expect(render(createElement(Popover, { label: "More", trigger: "open" }, "body"))).not.toContain(
      'role="dialog"',
    );
    expect(
      render(createElement(Dropdown, { label: "Actions", items: [], trigger: "open", children: null })),
    ).not.toContain('role="menu"');
    expect(
      render(createElement(ContextMenu, { label: "Context", items: [], children: "area" })),
    ).not.toContain('role="menu"');
  });
});
