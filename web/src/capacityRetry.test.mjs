import assert from "node:assert/strict";
import { it } from "vitest";
import { currentCapacityRetry } from "./capacityRetry.ts";

const agent = {
  threadId: "thread",
  epoch: 7,
  accountKey: "account",
  capacityRetry: {
    id: "retry",
    threadId: "thread",
    epoch: 7,
    accountKey: "account",
    status: null,
  },
};

it("returns generated retry data after validating its identity", () => {
  assert.equal(currentCapacityRetry(agent), agent.capacityRetry);
  assert.equal(currentCapacityRetry({ ...agent, threadId: "other" }), null);
  assert.equal(
    currentCapacityRetry({
      ...agent,
      capacityRetry: { ...agent.capacityRetry, epoch: 6 },
    }),
    null,
  );
  assert.equal(
    currentCapacityRetry({
      ...agent,
      capacityRetry: { ...agent.capacityRetry, id: null },
    }),
    null,
  );
});

it("does not infer a retry status from an absent or null status", () => {
  assert.equal(currentCapacityRetry(agent)?.status, null);
  assert.equal(
    currentCapacityRetry({
      ...agent,
      capacityRetry: { ...agent.capacityRetry, status: undefined },
    })?.status,
    undefined,
  );
});
