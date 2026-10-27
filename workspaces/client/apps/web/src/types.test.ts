import { AgentStatus } from "./generated/api";
import { expect, test } from "vitest";
import { statusLabel, type Agent } from "./types";

const statusLabels = [
  ["unknown", "unknown"],
  ["idle", "Ready"],
  ["queued", "Queued"],
  ["starting", "Starting"],
  ["running", "Working"],
  ["waiting", "Turn ended"],
  ["approval", "Needs an answer"],
  ["completed", "Complete"],
  ["interrupted", "Interrupted"],
  ["failed", "Failed"],
  ["blocked", "blocked"],
  ["paused", "Stopped"],
  ["parked", "Turn ended"],
  ["abandoned", "abandoned"],
  ["capacity-retry", "capacity-retry"],
  ["waiting_on_monitor", "Waiting on command"],
] satisfies [NonNullable<Agent["status"]>, string][];

test.each(statusLabels)(
  "preserves the label for agent status %s",
  (status, label) => {
    expect(statusLabel(status)).toBe(label);
  },
);

test("missing status has no label", () => {
  expect(statusLabel(undefined)).toBe("");
  expect(statusLabel(null)).toBe("");
});

test.each([
  ["thinking", "Thinking"],
  ["writing", "Writing"],
  ["tool", "Using tools"],
  ["retrying", "Codex is retrying"],
  ["auth", "Restoring sign-in"],
  ["error", "Codex reported an error"],
  ["new-runtime-phase", "Working"],
  [undefined, "Working"],
  ["", "Working"],
])("preserves the running phase label for %s", (phase, label) => {
  expect(statusLabel(AgentStatus.running, phase)).toBe(label);
});

test("phase and parked-event labels only override their own status", () => {
  expect(statusLabel(AgentStatus.idle, "thinking", "ready")).toBe("Ready");
  expect(statusLabel(AgentStatus.parked, "thinking", "ready")).toBe(
    "Waiting for event ready",
  );
  expect(statusLabel(AgentStatus.parked, "thinking", "")).toBe("Turn ended");
});
