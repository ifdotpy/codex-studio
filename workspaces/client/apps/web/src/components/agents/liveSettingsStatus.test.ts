import { expect, test } from "vitest";
import { observeSettingsChange } from "./liveSettingsStatus";

const change = {
  agentId: "worker",
  accountKey: "a",
  phase: "saving" as const,
  before: { model: "old", effort: "medium", fastMode: false },
  requested: { model: "new", effort: "high", fastMode: true },
};
const agent = {
  id: "worker",
  accountKey: "a",
  model: "old",
  effort: "medium",
  fastMode: false,
};
test("matching SSE updates do not confirm a request", () => {
  expect(
    observeSettingsChange(change, {
      ...agent,
      pendingSettings: change.requested,
    }).phase,
  ).toBe("saving");
  expect(
    observeSettingsChange(change, { ...agent, ...change.requested }).phase,
  ).toBe("saving");
});
test("an account change fails visibly", () => {
  expect(
    observeSettingsChange(change, { ...agent, accountKey: "b" }).phase,
  ).toBe("failed");
});
