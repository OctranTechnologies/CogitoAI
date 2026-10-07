import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { createElement } from "react";
import { PromptComposer } from "./prompt-composer";
import type { ModelSettings, PermissionSettings } from "../lib/settings";
import type { InputAttachment } from "../lib/rpc";

// The composer's real behaviour, driven through the DOM. Every control it shows
// is exercised here so a control cannot quietly stop working.
//
// Vitest does not expose globals, so Testing Library's automatic cleanup is not
// registered; without an explicit call each render leaks into the next test.
afterEach(cleanup);

const MODELS: ModelSettings = {
  provider: "openai",
  provider_id: "openai",
  model: "gpt-4o",
  base_url: "https://api.openai.com/v1",
  api_key_env: "OPENAI_API_KEY",
  capabilities: {
    text_input: true,
    image_input: false,
    streaming: true,
    tool_calling: true,
    parallel_tool_calls: false,
    vision: false,
    reasoning: true,
    configurable_reasoning_effort: false,
    context_window: 128000,
    max_output_tokens: null,
    structured_output: false,
  },
  credential: { available: true, source: "environment", env_var: "OPENAI_API_KEY" },
  available_models: ["gpt-4o", "gpt-4o-mini"],
  reasoning_levels: [],
  reasoning_effort: null,
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
  const onSelectTaskMode = vi.fn();
  const onAttachmentsChange = vi.fn();
  const props = {
    value: "",
    onChange,
    onSubmit,
    disabled: false,
    running: false,
    onCancel: vi.fn(),
    models: MODELS,
    modelCatalog: [],
    providerCredentials: [{ provider_id: "openai", provider: "OpenAI", credential: MODELS.credential }],
    isLoadingModelCatalog: false,
    permissions: PERMISSIONS,
    onSelectModel,
    onRefreshModelCatalog: vi.fn(),
    onConnectProvider: vi.fn(),
    onSelectReasoning: vi.fn(),
    onSelectMode,
    taskMode: "code" as const,
    onSelectTaskMode,
    pendingMode: null,
    workspacePath: "C:/repo",
    onChooseWorkspace: vi.fn(),
    size: "landing" as const,
    connected: true,
    onAttachmentsChange,
    ...overrides,
  };
  render(createElement(PromptComposer, props));
  const textarea = screen.getByLabelText("Message the agent") as HTMLTextAreaElement;
  return { textarea, onSubmit, onChange, onSelectModel, onSelectMode, onSelectTaskMode, onAttachmentsChange, props };
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
    expect(screen.getByText("OpenAI")).toBeTruthy();
    expect(screen.getByText("gpt-4o")).toBeTruthy();
  });

  it("groups and selects only models returned by the runtime catalog", async () => {
    const catalog = [
      {
        provider: "openai",
        id: "gpt-4o-mini",
        display_name: "GPT-4o mini",
        capabilities: MODELS.capabilities,
        metadata: {
          source: "discovered" as const,
          stale: false,
          refreshed_at_unix: null,
          reasoning_levels: null,
          capabilities: {
            text_input: "supported" as const,
            vision: "unsupported" as const,
            streaming: "supported" as const,
            tool_calling: "supported" as const,
            parallel_tool_calls: "unknown" as const,
            reasoning: "unknown" as const,
            configurable_reasoning_effort: "unknown" as const,
            structured_output: "unknown" as const,
          },
          pricing: null,
        },
      },
    ];
    const { onSelectModel } = setup({ modelCatalog: catalog });
    fireEvent.click(screen.getByRole("button", { name: /Choose model/ }));
    const option = await screen.findByRole("option", { name: /GPT-4o mini/ });
    fireEvent.click(option);
    expect(onSelectModel).toHaveBeenCalledWith("openai", "gpt-4o-mini");
  });

  it("shows disconnected provider state and routes connect and refresh actions", async () => {
    const { props } = setup({
      providerCredentials: [{
        provider_id: "openai",
        provider: "OpenAI",
        credential: { available: false, source: "none", env_var: "OPENAI_API_KEY" },
      }],
      modelCatalog: [{
        provider: "openai",
        id: "gpt-4o-mini",
        display_name: "GPT-4o mini",
        capabilities: MODELS.capabilities,
        metadata: {
          source: "discovered",
          stale: false,
          refreshed_at_unix: null,
          reasoning_levels: null,
          capabilities: {
            text_input: "supported",
            vision: "supported",
            streaming: "supported",
            tool_calling: "supported",
            parallel_tool_calls: "unknown",
            reasoning: "unknown",
            configurable_reasoning_effort: "unknown",
            structured_output: "unknown",
          },
          pricing: null,
        },
      }],
    });
    fireEvent.click(screen.getByRole("button", { name: /Choose model/ }));
    expect(screen.getAllByText("Not connected")).toHaveLength(5);
    fireEvent.click(screen.getByRole("button", { name: "Refresh OpenAI catalog" }));
    expect(props.onRefreshModelCatalog).toHaveBeenCalledWith("openai");
    fireEvent.click(screen.getByRole("button", { name: "Connect provider" }));
    expect(props.onConnectProvider).toHaveBeenCalledWith("openai");
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

  it("keeps task behavior separate from execution permission and selects PLAN", async () => {
    const { onSelectTaskMode } = setup();
    fireEvent.click(screen.getByRole("button", { name: "Task behavior: Code" }));
    expect(await screen.findByRole("menuitem", { name: /Explore.*Read and search only/ })).toBeTruthy();
    expect(screen.getByRole("menuitem", { name: /Plan.*read-only implementation plan/i })).toBeTruthy();
    expect(screen.getByRole("menuitem", { name: /Code.*Edit, run approved commands, and verify/ })).toBeTruthy();
    fireEvent.click(screen.getByRole("menuitem", { name: /^Plan/ }));
    expect(onSelectTaskMode).toHaveBeenCalledWith("plan");
    expect(screen.getByRole("button", { name: /^Execution mode:/ })).toBeTruthy();
  });

  it("does not open a menu for a selector with no runtime behind it", () => {
    setup({ connected: false, models: null, permissions: null });
    const model = screen.getByRole("button", { name: "Model" });
    const mode = screen.getByRole("button", { name: "Execution mode" });
    const taskMode = screen.getByRole("button", { name: "Task behavior" });
    fireEvent.click(model);
    fireEvent.click(mode);
    fireEvent.click(taskMode);
    expect(screen.queryByRole("menu")).toBeNull();
    expect(model.getAttribute("aria-disabled")).toBe("true");
    expect(taskMode.getAttribute("aria-disabled")).toBe("true");
  });

  it("does not render reasoning effort controls when the registry reports none", () => {
    setup();
    fireEvent.click(screen.getByRole("button", { name: /Choose model/ }));
    expect(screen.queryByRole("combobox", { name: "Reasoning effort" })).toBeNull();
  });

  it("filters the live catalog and moves model focus with the keyboard", async () => {
    const catalog = ["gpt-4o-mini", "gpt-4.1"].map((id) => ({
      provider: "openai",
      id,
      display_name: id,
      capabilities: MODELS.capabilities,
      metadata: {
        source: "discovered" as const,
        stale: false,
        refreshed_at_unix: null,
        reasoning_levels: null,
        capabilities: {
          text_input: "supported" as const,
          vision: "unsupported" as const,
          streaming: "supported" as const,
          tool_calling: "supported" as const,
          parallel_tool_calls: "unknown" as const,
          reasoning: "unknown" as const,
          configurable_reasoning_effort: "unknown" as const,
          structured_output: "unknown" as const,
        },
        pricing: null,
      },
    }));
    setup({ modelCatalog: catalog });
    fireEvent.click(screen.getByRole("button", { name: /Choose model/ }));
    const search = await screen.findByRole("textbox", { name: "Search models and providers" });
    fireEvent.change(search, { target: { value: "4.1" } });
    expect(screen.getByRole("option", { name: /gpt-4.1/ })).toBeTruthy();
    expect(screen.queryByRole("option", { name: /gpt-4o-mini/ })).toBeNull();
    fireEvent.keyDown(search, { key: "ArrowDown" });
    expect(document.activeElement).toBe(screen.getByRole("option", { name: /gpt-4.1/ }));
  });

  it("exposes only the selected model's advertised reasoning efforts", async () => {
    const descriptor = {
      provider: "openai",
      id: "gpt-4o",
      display_name: "GPT-4o",
      capabilities: MODELS.capabilities,
      metadata: {
        source: "discovered" as const,
        stale: false,
        refreshed_at_unix: null,
        reasoning_levels: ["low", "high"],
        capabilities: {
          text_input: "supported" as const,
          vision: "unsupported" as const,
          streaming: "supported" as const,
          tool_calling: "supported" as const,
          parallel_tool_calls: "unknown" as const,
          reasoning: "supported" as const,
          configurable_reasoning_effort: "supported" as const,
          structured_output: "unknown" as const,
        },
        pricing: null,
      },
    };
    const { props } = setup({ models: { ...MODELS, reasoning_levels: ["low", "high"] }, modelCatalog: [descriptor] });
    fireEvent.click(screen.getByRole("button", { name: /Choose model/ }));
    const effort = await screen.findByRole("combobox", { name: "Reasoning effort" });
    expect(Array.from((effort as HTMLSelectElement).options).map((option) => option.value)).toEqual([
      "off",
      "low",
      "high",
    ]);
    fireEvent.change(effort, { target: { value: "high" } });
    expect(props.onSelectReasoning).toHaveBeenCalledWith("high");
  });

  it("offers an active attachment picker and passes selected files to the parent", async () => {
    const { onAttachmentsChange } = setup();
    const attach = screen.getByRole("button", { name: "Attach files" }) as HTMLButtonElement;
    expect(attach.disabled).toBe(false);
    const picker = screen.getByLabelText("Choose attachments") as HTMLInputElement;
    const note = new File(["error details"], "error.txt", { type: "text/plain" });
    Object.defineProperty(picker, "files", { configurable: true, value: [note] });
    fireEvent.change(picker);
    await waitFor(() => expect(onAttachmentsChange).toHaveBeenCalledWith([
      { kind: "text", file_name: "error.txt", media_type: "text/plain", text: "error details" },
    ] satisfies InputAttachment[]));
  });

  it("submits preselected attachment metadata with the prompt", () => {
    const attachment: InputAttachment = {
      kind: "text",
      file_name: "notes.txt",
      media_type: "text/plain",
      text: "Relevant source excerpt",
    };
    const { onSubmit, textarea } = setup({ attachments: [attachment] });
    fireEvent.change(textarea, { target: { value: "Investigate" } });
    fireEvent.click(screen.getByRole("button", { name: "Send" }));
    expect(onSubmit).toHaveBeenCalledWith("Investigate", [attachment]);
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
        modelCatalog: [],
        providerCredentials: [],
        isLoadingModelCatalog: false,
        permissions: null,
        onSelectModel: vi.fn(),
        onRefreshModelCatalog: vi.fn(),
        onConnectProvider: vi.fn(),
        onSelectReasoning: vi.fn(),
        onSelectMode: vi.fn(),
        taskMode: "code",
        onSelectTaskMode: vi.fn(),
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
        modelCatalog: [],
        providerCredentials: [],
        isLoadingModelCatalog: false,
        permissions: null,
        onSelectModel: vi.fn(),
        onRefreshModelCatalog: vi.fn(),
        onConnectProvider: vi.fn(),
        onSelectReasoning: vi.fn(),
        onSelectMode: vi.fn(),
        taskMode: "code",
        onSelectTaskMode: vi.fn(),
        pendingMode: null,
        runtimeError: "runtime said no",
      }),
    );
    await waitFor(() => expect(screen.getByText(/runtime said no/)).toBeTruthy());
    fireEvent.click(screen.getByRole("button", { name: /dismiss notice/i }));
    await waitFor(() => expect(screen.queryByText(/runtime said no/)).toBeNull());
  });
});
