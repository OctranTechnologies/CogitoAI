import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { createElement } from "react";
import { PromptComposer } from "./prompt-composer";
import type { ModelSettings, PermissionSettings } from "../lib/settings";

// The composer's real behaviour, driven through the DOM. Every control it shows
// is exercised here so a control cannot quietly stop working.
//
// Vitest does not expose globals, so Testing Library's automatic cleanup is not
// registered; without an explicit call each render leaks into the next test.
afterEach(cleanup);

const MODELS: ModelSettings = {
  provider: "openai",
  model: "gpt-4o",
  base_url: "https://api.openai.com/v1",
  api_key_env: "OPENAI_API_KEY",
  capabilities: { streaming: true, tool_calling: true, vision: false, reasoning: true, context_window: 128000 },
  credential: { available: true, source: "environment", env_var: "OPENAI_API_KEY" },
  available_models: ["gpt-4o", "gpt-4o-mini"],
  configured: true,
};

const PERMISSIONS: PermissionSettings = {
  mode: "normal",
  mode_description: "Allows project edits and known-safe commands.",
  available_modes: ["read_only", "safe", "normal", "auto"],
  built_in_rules: [],
  configured_rules: [],
  default_behavior: [],
};

function setup(overrides: Partial<Parameters<typeof PromptComposer>[0]> = {}) {
  const onSubmit = vi.fn();
  const onChange = vi.fn();
  const onSelectModel = vi.fn();
  const onSelectMode = vi.fn();
  const props = {
    value: "",
    onChange,
    onSubmit,
    disabled: false,
    running: false,
    onCancel: vi.fn(),
    models: MODELS,
    permissions: PERMISSIONS,
    onSelectModel,
    onSelectMode,
    pendingMode: null,
    workspacePath: "C:/repo",
    onChooseWorkspace: vi.fn(),
    size: "landing" as const,
    connected: true,
    ...overrides,
  };
  render(createElement(PromptComposer, props));
  const textarea = screen.getByLabelText("Message the agent") as HTMLTextAreaElement;
  return { textarea, onSubmit, onChange, onSelectModel, onSelectMode, props };
}

describe("PromptComposer", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("submits through the send button", () => {
    const { textarea, onSubmit } = setup({ value: "fix the parser" });
    const send = screen.getByRole("button", { name: /send/i });
    fireEvent.click(send);
    expect(onSubmit).toHaveBeenCalledWith("fix the parser");
    expect(textarea).toBeTruthy();
  });

  it("submits on Enter", () => {
    const { textarea, onSubmit } = setup({ value: "run the tests" });
    fireEvent.keyDown(textarea, { key: "Enter", shiftKey: false });
    expect(onSubmit).toHaveBeenCalledWith("run the tests");
  });

  it("inserts a newline on Shift+Enter instead of submitting", () => {
    const { textarea, onSubmit } = setup({ value: "line one" });
    fireEvent.keyDown(textarea, { key: "Enter", shiftKey: true });
    expect(onSubmit).not.toHaveBeenCalled();
  });

  it("does not submit while an IME is composing", () => {
    const { textarea, onSubmit } = setup({ value: "日本語" });
    // Enter commits an IME candidate; submitting there would truncate input.
    fireEvent.keyDown(textarea, { key: "Enter", isComposing: true });
    expect(onSubmit).not.toHaveBeenCalled();
  });

  it("refuses to submit an empty or whitespace-only prompt", () => {
    for (const value of ["", "   ", "\n\t "]) {
      const { onSubmit } = setup({ value });
      const send = screen.getByRole("button", { name: /send/i }) as HTMLButtonElement;
      expect(send.disabled, `send should be disabled for ${JSON.stringify(value)}`).toBe(true);
      fireEvent.click(send);
      expect(onSubmit).not.toHaveBeenCalled();
      cleanup();
    }
  });

  it("reports every keystroke so the shell can grow the box", () => {
    const { textarea, onChange } = setup({ value: "" });
    fireEvent.change(textarea, { target: { value: "a" } });
    fireEvent.change(textarea, { target: { value: "ab" } });
    expect(onChange).toHaveBeenNthCalledWith(1, "a");
    expect(onChange).toHaveBeenNthCalledWith(2, "ab");
  });

  it("accepts a very long prompt without breaking the control row", () => {
    const long = "line\n".repeat(2000);
    const { textarea, onSubmit } = setup({ value: long });
    expect(textarea.value).toHaveLength(long.length);
    fireEvent.click(screen.getByRole("button", { name: /send/i }));
    expect(onSubmit).toHaveBeenCalledTimes(1);
  });

  it("trims surrounding whitespace before submitting", () => {
    const { onSubmit } = setup({ value: "   do the thing   " });
    fireEvent.click(screen.getByRole("button", { name: /send/i }));
    expect(onSubmit).toHaveBeenCalledWith("do the thing");
  });

  it("shows the active model as provider and model", () => {
    setup();
    expect(screen.getByText("openai")).toBeTruthy();
    expect(screen.getByText("gpt-4o")).toBeTruthy();
  });

  it("offers only the models the runtime advertises", async () => {
    const { onSelectModel } = setup();
    fireEvent.click(screen.getByRole("button", { name: /^Model: openai/ }));
    const option = await screen.findByRole("menuitem", { name: /gpt-4o-mini/ });
    fireEvent.click(option);
    expect(onSelectModel).toHaveBeenCalledWith("gpt-4o-mini");
  });

  it("labels every execution mode for people rather than for the runtime", async () => {
    const { onSelectMode } = setup();
    fireEvent.click(screen.getByRole("button", { name: /^Execution mode: Normal/ }));
    for (const label of ["Read Only", "Safe", "Auto"]) {
      expect(await screen.findByRole("menuitem", { name: new RegExp(label) })).toBeTruthy();
    }
    fireEvent.click(screen.getByRole("menuitem", { name: /Read Only/ }));
    expect(onSelectMode).toHaveBeenCalledWith("read_only");
  });

  it("does not open a menu for a selector with no runtime behind it", () => {
    setup({ connected: false, models: null, permissions: null });
    const model = screen.getByRole("button", { name: "Model" });
    const mode = screen.getByRole("button", { name: "Execution mode" });
    fireEvent.click(model);
    fireEvent.click(mode);
    expect(screen.queryByRole("menu")).toBeNull();
    expect(model.getAttribute("aria-disabled")).toBe("true");
  });

  it("reports the runtime's capabilities without inventing an effort control", () => {
    setup();
    const trigger = screen.getByRole("button", { name: /^Model: openai/ });
    expect(trigger.textContent).toContain("Supports tool calling, reasoning.");
    expect(screen.queryByRole("button", { name: /effort|thinking|reasoning level/i })).toBeNull();
  });

  it("marks attachment as unavailable rather than offering a dead button", () => {
    setup();
    const attach = screen.getByRole("button", { name: /attach files/i }) as HTMLButtonElement;
    expect(attach.disabled).toBe(true);
    expect(attach.getAttribute("aria-label")).toContain("not available");
  });

  it("offers a workspace switch above the textarea", () => {
    const { props } = setup();
    expect(screen.getByText("C:/repo")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: /change/i }));
    expect(props.onChooseWorkspace).toHaveBeenCalled();
  });

  it("shows Stop instead of Send while a run is in flight", () => {
    const { onSubmit } = setup({ running: true });
    const stop = screen.getByRole("button", { name: /stop/i });
    expect(stop).toBeTruthy();
    expect(screen.queryByRole("button", { name: /send/i })).toBeNull();
    fireEvent.click(stop);
    expect(onSubmit).not.toHaveBeenCalled();
  });

  it("prompts for the workspace when none is chosen", () => {
    setup({ workspacePath: "" });
    const textarea = screen.getByLabelText("Message the agent") as HTMLTextAreaElement;
    expect(textarea.placeholder).toMatch(/choose a workspace/i);
  });

  it("keeps the landing placeholder", () => {
    setup();
    const textarea = screen.getByLabelText("Message the agent") as HTMLTextAreaElement;
    expect(textarea.placeholder).toBe("Do anything in this workspace");
  });
});

describe("LandingView notice", () => {
  it("warns when no runtime is connected and lets a real error be dismissed", async () => {
    const { LandingView } = await import("./landing-view");
    const { rerender } = render(
      createElement(LandingView, {
        value: "",
        onChange: vi.fn(),
        onSubmit: vi.fn(),
        disabled: true,
        running: false,
        onCancel: vi.fn(),
        connected: false,
        workspacePath: "",
        onChooseWorkspace: vi.fn(),
        models: null,
        permissions: null,
        onSelectModel: vi.fn(),
        onSelectMode: vi.fn(),
        pendingMode: null,
        runtimeError: null,
      }),
    );
    await waitFor(() => expect(screen.getByText(/no runtime connected/i)).toBeTruthy());

    rerender(
      createElement(LandingView, {
        value: "",
        onChange: vi.fn(),
        onSubmit: vi.fn(),
        disabled: true,
        running: false,
        onCancel: vi.fn(),
        connected: true,
        workspacePath: "C:/repo",
        onChooseWorkspace: vi.fn(),
        models: null,
        permissions: null,
        onSelectModel: vi.fn(),
        onSelectMode: vi.fn(),
        pendingMode: null,
        runtimeError: "runtime said no",
      }),
    );
    await waitFor(() => expect(screen.getByText(/runtime said no/)).toBeTruthy());
    fireEvent.click(screen.getByRole("button", { name: /dismiss notice/i }));
    await waitFor(() => expect(screen.queryByText(/runtime said no/)).toBeNull());
  });
});
