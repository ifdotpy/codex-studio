import type { Agent, JsonValue } from "./types";

function jsonRecord(
  value: JsonValue | null | undefined,
): Record<string, JsonValue> | null {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? value
    : null;
}

export function currentCapacityRetry(
  agent: Agent,
): Record<string, JsonValue> | null {
  const retry = jsonRecord(agent.capacityRetry);
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
