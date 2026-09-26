import { describe, expect, it, vi } from "vitest";
import { renderToString } from "react-dom/server";
import { createElement } from "react";

// The shell reaches Tauri for the folder picker and for the RPC transport, so
// both are stubbed. Rendering is the point: a layout refactor that leaves a
// region out, or an unresolvable class, shows up as a failure here rather than as
// a blank window.
vi.mock("@tauri-apps/plugin-dialog", () => ({ open: vi.fn(async () => null) }));
vi.mock("./lib/rpc", async (importOriginal) => {
  const actual = await importOriginal<typeof import("./lib/rpc")>();
  return {
    ...actual,
    requestRuntime: vi.fn(async () => {
      throw new Error("not connected");
    }),
    receiveRuntimeMessage: vi.fn(() => {
      throw new Error("not connected");
    }),
    connectRuntime: vi.fn(async () => "client-test"),
    disconnectRuntime: vi.fn(async () => undefined),
  };
});

const { default: App } = await import("./App");

function render() {
  return renderToString(createElement(App));
}

describe("App shell layout", () => {
  it("renders the three regions", () => {
    const html = render();
    // Rail, sidebar, and the main workspace landmark.
    expect(html).toContain('aria-label="Primary"');
    expect(html).toContain("CogitoAI");
    expect(html).toContain("New task");
    expect(html).toContain("Projects");
    expect(html).toContain("Recents");
    expect(html).toContain("aria-label=\"Message the agent\"");
  });

  it("exposes every rail destination with an accessible name", () => {
    const html = render();
    for (const label of [
      "Home",
      "Sessions and history",
      "Projects",
      "Agent activity",
      "Models and integrations",
      "Settings",
    ]) {
      expect(html).toContain(`aria-label="${label}"`);
    }
  });

  it("marks the active rail entry as selected and leaves the others unselected", () => {
    const html = render();
    // The home entry is selected on first render.
    expect(html).toMatch(/aria-selected="true"[^>]*aria-label="Home"|aria-label="Home"[^>]*aria-selected="true"/);
    expect(html).toContain('aria-selected="false"');
  });

  it("sizes the rail and sidebar within the specified bounds", () => {
    const html = render();
    // Rail 48-56px and sidebar 280-320px, expressed as arbitrary widths.
    expect(html).toContain("w-[52px]");
    expect(html).toContain("w-[296px]");
  });

  it("keeps the shell full height and clips its own overflow", () => {
    const html = render();
    expect(html).toContain("h-screen");
    // Without this a wide child can push a horizontal scrollbar onto the window.
    expect(html).toContain("overflow-hidden");
  });

  it("lets the sidebar body scroll independently of the header", () => {
    const html = render();
    // The scroll container sits inside the sidebar, and the sidebar header is a
    // sibling rather than a child of it.
    expect(html).toContain("scroll-area");
    expect(html).toContain("border-r border-line bg-panel");
  });

  it("renders disconnected state rather than assuming a runtime", () => {
    // The landing screen is shown with no runtime, and it says so plainly
    // instead of presenting a composer that could never submit anything.
    const html = render();
    expect(html).toContain("What should we build?");
    expect(html).toContain("No runtime connected");
  });

  it("does not leak a raw palette value into the markup", () => {
    expect(render()).not.toMatch(/style="[^"]*#[0-9a-f]{6}/i);
  });

  it("invents no project or session data while disconnected", () => {
    const html = render();
    // With no runtime there is nothing to show, and nothing fabricated to show
    // in its place.
    expect(html).toContain("Connect a runtime to open a project.");
    expect(html).toContain("No sessions yet.");
  });
});

/**
 * The rail and sidebar are fixed-width, so the space left for the workspace is
 * whatever the window has left over. That arithmetic is easy to break by nudging
 * a width and impossible to notice without opening the app, so it is asserted
 * against the same minimum window size the Tauri shell declares.
 */
describe("fixed region sizing", () => {
  const RAIL = { min: 48, max: 56, actual: 52 };
  const SIDEBAR = { min: 280, max: 320, actual: 296 };
  // src-tauri/tauri.conf.json: the window cannot be resized below this.
  const MIN_WINDOW_WIDTH = 1024;
  // The context panel is `hidden` below the xl breakpoint, so it only competes
  // for space once the window is at least this wide.
  const XL_BREAKPOINT = 1280;
  const CONTEXT_PANEL = 384;

  it("keeps both fixed regions inside the specified bounds", () => {
    expect(RAIL.actual).toBeGreaterThanOrEqual(RAIL.min);
    expect(RAIL.actual).toBeLessThanOrEqual(RAIL.max);
    expect(SIDEBAR.actual).toBeGreaterThanOrEqual(SIDEBAR.min);
    expect(SIDEBAR.actual).toBeLessThanOrEqual(SIDEBAR.max);
  });

  it("leaves the workspace usable at the smallest supported window", () => {
    const workspace = MIN_WINDOW_WIDTH - RAIL.actual - SIDEBAR.actual;
    // A conversation needs a usable column, not a sliver.
    expect(workspace).toBeGreaterThanOrEqual(480);
  });

  it("leaves a usable column for the conversation once the context panel appears", () => {
    const withContext = XL_BREAKPOINT - RAIL.actual - SIDEBAR.actual - CONTEXT_PANEL;
    expect(withContext).toBeGreaterThanOrEqual(480);
  });

  it("renders the widths the arithmetic above assumes", () => {
    const html = render();
    expect(html).toContain(`w-[${RAIL.actual}px]`);
    expect(html).toContain(`w-[${SIDEBAR.actual}px]`);
  });
});
