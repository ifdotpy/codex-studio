import type { Agent } from "../../types";

type Values = {
  model: string | null;
  effort: string | null;
  fastMode: boolean;
};
export type SettingsChange = {
  agentId: string;
  accountKey: string;
  phase: "saving" | "queued" | "applied" | "failed";
  before: Values;
  requested: Values;
  error?: string;
};
const equal = (a: Values, b: Values) =>
  a.model === b.model && a.effort === b.effort && a.fastMode === b.fastMode;
const values = (value: {
  model?: string | null | undefined;
  effort?: string | null | undefined;
  fastMode?: boolean | null | undefined;
}): Values => ({
  model: value.model ?? null,
  effort: value.effort ?? null,
  fastMode: !!value.fastMode,
});
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
  if (change.phase === "applied" || change.phase === "failed") return change;
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
  if (agent.pendingSettings) {
    if (equal(values(agent.pendingSettings), change.requested))
      return change.phase === "queued"
        ? change
        : { ...change, phase: "queued" };
    if (change.phase === "queued")
      return {
        ...change,
        phase: "failed",
        error: "The queued settings were replaced before they applied.",
      };
    return change;
  }
  if (equal(values(agent), change.requested))
    return { ...change, phase: "applied" };
  if (change.phase === "queued")
    return {
      ...change,
      phase: "failed",
      error: "The queued settings were removed before they applied.",
    };
  return change;
}
