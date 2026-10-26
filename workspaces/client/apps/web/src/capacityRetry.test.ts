import type { Agent } from "./types";
import assert from "node:assert/strict";
import { it } from "vitest";
import { currentCapacityRetry as readCapacityRetry } from "./capacityRetry.ts";

type RetryFixture = Omit<
  Partial<NonNullable<Agent["capacityRetry"]>>,
  "status"
> & { status?: string | null };
type AgentFixture = Omit<Partial<Agent>, "capacityRetry"> & {
  capacityRetry?: RetryFixture;
};
// This partial fixture intentionally omits agent id, dueAt and updatedAt and allows malformed status.
const currentCapacityRetry = readCapacityRetry as (
  agent: AgentFixture,
) => RetryFixture | null;
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
} satisfies AgentFixture;

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
  assert.equal(currentCapacityRetry(agent)?.status, null);
  assert.equal(
    currentCapacityRetry({
      ...agent,
      capacityRetry: { ...agent.capacityRetry, status: undefined },
    })?.status,
    undefined,
  );
});
