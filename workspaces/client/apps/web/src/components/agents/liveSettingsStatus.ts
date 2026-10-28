import type { Agent } from "../../types";

type Values = {
  model: string | null;
  effort: string | null;
  fastMode: boolean;
};
export type SettingsChange = {
  agentId: string;
  accountKey: string;
  phase: "saving" | "queued" | "saved" | "failed";
  before: Values;
  requested: Values;
  error?: string;
  requestKey?: string;
  errorLogged?: boolean;
};
export function observeSettingsChange(
  change: SettingsChange,
  agent: Pick<
    Agent,
    | "id"
    | "accountKey"
    | "model"
    | "effort"
    | "fastMode"
    | "pendingSettings"
    | "pendingSettingsAccountKey"
  >,
): SettingsChange {
  if (change.phase === "failed" || change.phase === "saved") return change;
  if (
    agent.id !== change.agentId ||
    (agent.accountKey || "default") !== change.accountKey ||
    (agent.pendingSettingsAccountKey &&
      agent.pendingSettingsAccountKey !== change.accountKey)
  )
    return {
      ...change,
      phase: "failed",
      error: "The settings account changed before the change applied.",
    };
  return change;
}
