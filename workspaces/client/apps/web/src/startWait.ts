import { StartWaitReason } from "./generated/api";
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
    case StartWaitReason.slot:
      return `Waiting for an account slot (${wait.runningTurns}/${wait.maxRunningTurns})`;
    case StartWaitReason.pause:
      return "Waiting for the account pause";
    case StartWaitReason.start_queue:
      return "Waiting behind an earlier account start";
    case StartWaitReason.disk:
      return "Waiting for free disk space";
    case StartWaitReason.model_catalog:
      return "Waiting for the model catalog";
    case StartWaitReason.provider_session:
      return "Waiting for the provider session";
    case StartWaitReason.dependency:
      return "Waiting for a preparation dependency";
    case StartWaitReason.dispatch_pending:
      return "Waiting for dispatch handoff";
    case StartWaitReason.paused:
      return "Automatic continuation is paused";
    case StartWaitReason.native_failure:
      return "Waiting for native failure recovery";
    case StartWaitReason.account_transfer:
      return "Waiting for account transfer";
  }
  const unreachable: never = wait;
  return unreachable;
}
