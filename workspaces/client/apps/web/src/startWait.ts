import type { Agent } from "./types";

/**
 * Why a queued turn waits for its account's start throttle, or null. The
 * server writes `startWait` when the start waits and clears it with the
 * claim; it applies only while the agent is still queued at that epoch.
 */
export function startWaitLabel(agent: Agent): string | null {
  const wait = agent.startWait;
  if (!wait || agent.status !== "queued" || wait.epoch !== agent.epoch)
    return null;
  return wait.reason === "slot"
    ? `Waiting for an account slot (${wait.runningTurns}/${wait.maxRunningTurns})`
    : "Waiting for the account pause";
}
