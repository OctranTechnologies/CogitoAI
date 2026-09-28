import { afterEach, describe, expect, it } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import { createElement } from "react";
import { SessionHeader } from "./session-header";

afterEach(cleanup);

describe("session header layout", () => {
  it("keeps runtime changes on one stable line and retains full labels for tooltips", () => {
    const projectName = "C:/Users/Developer/Documents/GitHub/OctranTechnologies/CogitoAI";
    const branch = "feature/a-very-long-branch-name-for-a-coding-agent-interface";

    const { container } = render(
      createElement(SessionHeader, {
        projectName,
        branch,
        connected: true,
        runPhase: "running",
        models: {
          provider: "anthropic",
          provider_id: "anthropic",
          model: "claude-sonnet-long-context-model",
          base_url: "https://api.anthropic.com/v1",
          api_key_env: "ANTHROPIC_API_KEY",
          capabilities: {
            text_input: true,
            image_input: false,
            streaming: true,
            tool_calling: true,
            parallel_tool_calls: false,
            vision: false,
            reasoning: true,
            configurable_reasoning_effort: false,
            context_window: 200_000,
            max_output_tokens: null,
            structured_output: false,
          },
          credential: { available: true, source: "environment", env_var: "ANTHROPIC_API_KEY" },
          available_models: ["claude-sonnet-long-context-model"],
          reasoning_levels: [],
          reasoning_effort: null,
          configured: true,
        },
        permissions: {
          mode: "normal",
          mode_description: "Normal execution",
          available_modes: ["read_only", "safe", "normal", "auto"],
          built_in_rules: [],
          configured_rules: [],
          default_behavior: [],
        },
        activeTools: 3,
        failures: 1,
      }),
    );

    const header = container.querySelector("header");
    for (const className of ["h-9", "flex-nowrap", "overflow-hidden", "flex-1"]) {
      expect(header?.classList.contains(className)).toBe(true);
    }
    expect(screen.getByTestId("header-project").getAttribute("title")).toBe(projectName);
    expect(screen.getByTestId("header-branch").getAttribute("title")).toBe(branch);
    expect(screen.getByTestId("header-active-tools").textContent).toContain("3 in flight");
    expect(screen.getByTestId("header-failures").textContent).toContain("1 failed");
  });
});
