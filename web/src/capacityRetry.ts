import type { Agent } from "./types";

export type CapacityRetry = NonNullable<Agent["capacityRetry"]>;

export function currentCapacityRetry(agent: Agent): CapacityRetry | null {
  const retry = agent.capacityRetry;
  if (
    !retry ||
    retry.threadId !== agent.threadId ||
    retry.epoch !== agent.epoch ||
    (retry.accountKey || "default") !== (agent.accountKey || "default") ||
    retry.status === "finished" ||
    retry.acceptedTurnId
  )
    return null;
  return retry;
}
