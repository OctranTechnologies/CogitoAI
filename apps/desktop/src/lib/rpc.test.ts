import { describe, expect, it } from "vitest";
import { parseServerMessage } from "./rpc";

describe("parseServerMessage", () => {
  it("parses versioned responses", () => {
    const message = parseServerMessage({ version: 1, id: "request-1", ok: true, result: { ready: true } });
    expect(message).toEqual({
      kind: "response",
      response: { version: 1, id: "request-1", ok: true, result: { ready: true }, error: undefined },
    });
  });

  it("parses runtime notifications", () => {
    const message = parseServerMessage({ version: 1, method: "agent.completed", params: { run_id: "run-1" } });
    expect(message.kind).toBe("notification");
    if (message.kind === "notification") expect(message.notification.method).toBe("agent.completed");
  });

  it("rejects malformed and unsupported messages", () => {
    expect(() => parseServerMessage({ version: 2, id: null, ok: true })).toThrow(/unsupported/);
    expect(() => parseServerMessage({ version: 1, unexpected: true })).toThrow(/unknown/);
    expect(() => parseServerMessage(null)).toThrow(/malformed/);
  });
});
