import type { Agent } from "./types";

/** Render the server's epoch-scoped wait, shared with orchestration_status. */
export function startWaitLabel(agent: Agent): string | null {
  const wait = agent.startWait;
  if (
    !wait ||
    !["queued", "starting"].includes(agent.status ?? "") ||
    wait.epoch !== agent.epoch
  )
    return null;
  switch (wait.reason) {
    case "slot":
      return `Waiting for an account slot (${wait.runningTurns}/${wait.maxRunningTurns})`;
    case "pause":
      return "Waiting for the account pause";
    case "start_queue":
      return "Waiting behind an earlier account start";
    case "disk":
      return "Waiting for free disk space";
    case "model_catalog":
      return "Waiting for the model catalog";
    case "provider_session":
      return "Waiting for the provider session";
    case "dependency":
      return "Waiting for a preparation dependency";
    case "dispatch_pending":
      return "Waiting for dispatch handoff";
    case "paused":
      return "Automatic continuation is paused";
    case "native_failure":
      return "Waiting for native failure recovery";
    case "account_transfer":
      return "Waiting for account transfer";
  }
}
