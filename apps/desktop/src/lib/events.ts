import type { HarnessEvent } from "./rpc";

export interface ChatMessage {
  id: string;
  role: "user" | "assistant" | "system";
  text: string;
  createdAt: number;
  streaming?: boolean;
}

export type ToolActivityState =
  | "requested"
  | "awaiting_approval"
  | "approved"
  | "running"
  | "succeeded"
  | "failed"
  | "denied";

export interface ToolActivity {
  id: string;
  name: string;
  target: string;
  state: ToolActivityState;
  durationMs: number | null;
  output: string;
  error: string;
  timestamp: number;
}

export type VerificationState = "running" | "passed" | "failed";

export interface VerificationActivity {
  id: string;
  command: string;
  category: string;
  state: VerificationState;
  passed: boolean | null;
  exitCode: number | null;
  durationMs: number | null;
  diagnostics: string[];
}

export interface TimelineEntry {
  id: string;
  eventType: string;
  label: string;
  detail: string;
  timestamp: number;
  tone: "neutral" | "success" | "warning" | "danger";
}

export type RunPhase =
  | "idle"
  | "pending"
  | "running"
  | "cancelling"
  | "completed"
  | "failed"
  | "cancelled";

const TIMELINE_EVENTS = new Set([
  "user.message",
  "assistant.message",
  "tool.requested",
  "tool.approved",
  "tool.denied",
  "tool.completed",
  "tool.failed",
  "file.changed",
  "checkpoint.created",
  "checkpoint.restored",
  "verification.started",
  "verification.result",
  "context.compacted",
  "session.resumed",
  "session.completed",
  "session.failed",
]);

function text(event: HarnessEvent): string {
  return typeof event.payload.data.text === "string" ? event.payload.data.text : "";
}

function stringField(event: HarnessEvent, key: string): string {
  const value = event.payload.data[key];
  return typeof value === "string" ? value : "";
}

function stringList(event: HarnessEvent, key: string): string[] {
  const value = event.payload.data[key];
  return Array.isArray(value) ? value.filter((item): item is string => typeof item === "string") : [];
}

function activeTool(activities: ToolActivity[], name: string): ToolActivity | undefined {
  return [...activities]
    .reverse()
    .find((activity) => activity.name === name && !["succeeded", "failed", "denied"].includes(activity.state));
}

export function deriveToolActivities(events: HarnessEvent[]): ToolActivity[] {
  const activities: ToolActivity[] = [];
  for (const event of events) {
    const name = stringField(event, "tool");
    if (!name) continue;
    if (event.event_type === "tool.requested") {
      const args = event.payload.data.arguments;
      const record = args && typeof args === "object" ? (args as Record<string, unknown>) : {};
      const target =
        (typeof record.path === "string" && record.path) ||
        (typeof record.command === "string" && record.command) ||
        (typeof record.working_directory === "string" && record.working_directory) ||
        "";
      activities.push({
        id: event.event_id,
        name,
        target,
        state: "requested",
        durationMs: null,
        output: "",
        error: "",
        timestamp: event.timestamp,
      });
      continue;
    }
    const activity = activeTool(activities, name);
    if (!activity) continue;
    if (event.event_type === "tool.approved") activity.state = "approved";
    if (event.event_type === "tool.denied") {
      activity.state = "denied";
      activity.error = stringField(event, "reason");
    }
    if (event.event_type === "tool.started") {
      activity.state = "running";
      activity.timestamp = event.timestamp;
    }
    if (event.event_type === "tool.output") activity.output = stringField(event, "output");
    if (event.event_type === "tool.completed") {
      activity.state = "succeeded";
      activity.durationMs = Math.max(0, event.timestamp - activity.timestamp);
    }
    if (event.event_type === "tool.failed") {
      activity.state = "failed";
      activity.error = stringField(event, "error");
      activity.durationMs = Math.max(0, event.timestamp - activity.timestamp);
    }
  }
  return activities.slice(-40);
}

export function deriveVerificationActivities(events: HarnessEvent[]): VerificationActivity[] {
  const activities: VerificationActivity[] = [];
  for (const event of events) {
    if (event.event_type === "verification.started") {
      for (const command of stringList(event, "commands")) {
        activities.push({
          id: `${event.event_id}:${command}`,
          command,
          category: "pending",
          state: "running",
          passed: null,
          exitCode: null,
          durationMs: null,
          diagnostics: [],
        });
      }
      continue;
    }
    if (event.event_type !== "verification.result") continue;
    const command = stringField(event, "command");
    const exitCode = event.payload.data.exit_code;
    const duration = event.payload.data.duration_ms;
    const passed = event.payload.data.passed === true;
    const existing = [...activities].reverse().find((item) => item.command === command && item.state === "running");
    const result: VerificationActivity = {
      id: event.event_id,
      command,
      category: stringField(event, "category"),
      state: passed ? "passed" : "failed",
      passed,
      exitCode: typeof exitCode === "number" ? exitCode : null,
      durationMs: typeof duration === "number" ? duration : null,
      diagnostics: stringList(event, "diagnostics"),
    };
    if (existing) Object.assign(existing, result, { id: existing.id });
    else activities.push(result);
  }
  return activities.slice(-20);
}

export function hydrateConversation(events: HarnessEvent[]): ChatMessage[] {
  return events
    .filter((event) => event.event_type === "user.message" || event.event_type === "assistant.message")
    .map((event) => ({
      id: event.event_id,
      role: event.event_type === "user.message" ? ("user" as const) : ("assistant" as const),
      text: text(event),
      createdAt: event.timestamp,
    }));
}

export function deriveTimeline(events: HarnessEvent[]): TimelineEntry[] {
  return events
    .filter((event) => TIMELINE_EVENTS.has(event.event_type))
    .map((event) => {
      const failed = event.event_type.endsWith(".failed") || event.event_type === "tool.denied";
      const completed = event.event_type.endsWith(".completed") || event.event_type === "checkpoint.restored";
      const warning = event.event_type === "tool.approved" || event.event_type === "verification.started";
      const detail =
        text(event) ||
        stringField(event, "tool") ||
        stringField(event, "command") ||
        stringField(event, "path") ||
        stringField(event, "error") ||
        stringField(event, "reason");
      return {
        id: event.event_id,
        eventType: event.event_type,
        label: event.event_type,
        detail,
        timestamp: event.timestamp,
        tone: failed ? ("danger" as const) : completed ? ("success" as const) : warning ? ("warning" as const) : ("neutral" as const),
      };
    })
    .slice(-60);
}
