import type { Agent, Json } from "./types";

export function currentCapacityRetry(agent: Agent): Json | null {
  const retry = agent.capacityRetry;
  if (
    !retry ||
    typeof retry.id !== "string" ||
    retry.threadId !== agent.threadId ||
    retry.epoch !== agent.epoch ||
    (retry.accountKey || "default") !== (agent.accountKey || "default") ||
    retry.status === "finished" ||
    retry.acceptedTurnId
  )
    return null;
  return retry;
}
