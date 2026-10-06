import type { Agent } from "./types";

export type CapacityRetry = NonNullable<Agent["capacityRetry"]> & {
  id: string;
};

function hasRetryId(
  retry: NonNullable<Agent["capacityRetry"]>,
): retry is CapacityRetry {
  return typeof retry.id === "string";
}

export function currentCapacityRetry(agent: Agent): CapacityRetry | null {
  const retry = agent.capacityRetry;
  if (
    !retry ||
    !hasRetryId(retry) ||
    retry.threadId !== agent.threadId ||
    retry.epoch !== agent.epoch ||
    (retry.accountKey || "default") !== (agent.accountKey || "default") ||
    retry.status === "finished" ||
    retry.acceptedTurnId
  )
    return null;
  return retry;
}
