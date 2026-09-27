import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { CommandMenu, type CommandItem } from "./command-menu";

afterEach(cleanup);

const items: CommandItem[] = [
  { id: "new", label: "New task", group: "Tasks", onSelect: vi.fn() },
  { id: "search", label: "Search sessions", group: "Sessions", shortcut: "Ctrl+P", onSelect: vi.fn() },
];

describe("command menu keyboard interaction", () => {
  it("focuses search, moves the active option with arrows, and runs it with Enter", () => {
    render(<CommandMenu open onClose={vi.fn()} items={items} label="Command palette" />);
    const input = screen.getByRole("textbox", { name: "Command palette" });
    expect(document.activeElement).toBe(input);

    fireEvent.keyDown(input, { key: "ArrowDown" });
    expect(input.getAttribute("aria-activedescendant")).toBe("command-option-search");
    fireEvent.keyDown(input, { key: "Enter" });
    expect(items[1].onSelect).toHaveBeenCalledOnce();
  });

  it("filters commands and closes on Escape", () => {
    const onClose = vi.fn();
    render(<CommandMenu open onClose={onClose} items={items} label="Command palette" />);
    const input = screen.getByRole("textbox", { name: "Command palette" });
    fireEvent.change(input, { target: { value: "search" } });
    expect(screen.getAllByRole("option")).toHaveLength(1);
    fireEvent.keyDown(document, { key: "Escape" });
    expect(onClose).toHaveBeenCalledOnce();
  });
});
