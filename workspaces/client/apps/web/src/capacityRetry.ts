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

// The cause of a capacity retry that waits for free disk space.
export const DISK_FULL_CAUSE = "diskFull";
export const DISK_FULL_WAIT_LABEL = "Waiting for disk space";

// A held agent whose turn failed because the disk was full.
export function diskFullHold(agent: Agent): CapacityRetry | null {
  const retry = currentCapacityRetry(agent);
  return retry?.cause === DISK_FULL_CAUSE && retry.status === "scheduled"
    ? retry
    : null;
}
