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
test("SSE confirms queued then applied settings without changing permissions", () => {
  const queued = observeSettingsChange(change, {
    ...agent,
    pendingSettings: change.requested,
  });
  expect(queued.phase).toBe("queued");
  const updatedAgent = { ...agent, ...change.requested, yoloMode: false };
  const applied = observeSettingsChange(queued, updatedAgent);
  expect(updatedAgent.yoloMode).toBe(false);
  expect(applied.phase).toBe("applied");
});
test("an account change or replaced queued settings fails visibly", () => {
  const queued = observeSettingsChange(change, {
    ...agent,
    pendingSettings: change.requested,
  });
  expect(
    observeSettingsChange(queued, { ...agent, accountKey: "b" }).phase,
  ).toBe("failed");
  expect(
    observeSettingsChange(queued, {
      ...agent,
      pendingSettings: { ...change.requested, model: "other" },
    }).phase,
  ).toBe("failed");
});
test("old SSE state does not claim a result while saving", () => {
  expect(observeSettingsChange(change, agent).phase).toBe("saving");
});
