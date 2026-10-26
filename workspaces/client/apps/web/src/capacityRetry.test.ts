import type { Agent } from "./types";
import assert from "node:assert/strict";
import { it } from "vitest";
import { currentCapacityRetry } from "./capacityRetry.ts";

const agent = {
  id: "agent",
  threadId: "thread",
  epoch: 7,
  accountKey: "account",
  capacityRetry: {
    dueAt: null,
    updatedAt: 0,
    id: "retry",
    threadId: "thread",
    epoch: 7,
    accountKey: "account",
    status: "scheduled",
  },
} satisfies Agent;

it("returns generated retry data after validating its thread and epoch", () => {
  assert.equal(currentCapacityRetry(agent), agent.capacityRetry);
  assert.equal(currentCapacityRetry({ ...agent, threadId: "other" }), null);
  assert.equal(
    currentCapacityRetry({
      ...agent,
      capacityRetry: { ...agent.capacityRetry, epoch: 6 },
    }),
    null,
  );
});

it("does not infer a retry status from an absent or null status", () => {
  assert.equal(
    currentCapacityRetry({
      ...agent,
      capacityRetry: {
        ...agent.capacityRetry,
        // @ts-expect-error status is deliberately null in this malformed retry.
        status: null,
      },
    })?.status,
    null,
  );
  assert.equal(
    currentCapacityRetry({
      ...agent,
      // @ts-expect-error status is deliberately undefined in this malformed retry.
      capacityRetry: { ...agent.capacityRetry, status: undefined },
    })?.status,
    undefined,
  );
});
