import type { components } from "../generated/api";

// This cache receives volatile SSE events. It never enters the sync database.
export type TokenRate = components["schemas"]["TokenRateValue"];
type TokenRateEvent = components["schemas"]["ResourceTokenRatesEvent"];
let watchStream:
  | ((listener: (event: TokenRateEvent) => void) => () => void)
  | undefined;
export function configureTokenRateStream(
  watch: (listener: (event: TokenRateEvent) => void) => () => void,
) {
  watchStream = watch;
}
const values = new Map<string, TokenRate | null>();
const listeners = new Map<string, Set<(value: TokenRate | null) => void>>();
const teamMembers = new Map<string, Set<string>>();
function isRecord(value: unknown): value is Record<string, unknown> {
  return !!value && typeof value === "object" && !Array.isArray(value);
}
function publishRate(id: string, value: TokenRate | null) {
  values.delete(id);
  values.set(id, value);
  if (values.size > 2048) values.delete(values.keys().next().value!);
  for (const listener of listeners.get(id) || []) listener(value);
}
export const workerRateKey = (teamId: string, agentId: string) =>
  `team:${teamId}:${agentId}`;
export function clearTeamTokenRates(teamId: string) {
  const members = teamMembers.get(teamId);
  if (members)
    for (const id of members) {
      const key = workerRateKey(teamId, id);
      publishRate(key, null);
      values.delete(key);
    }
  teamMembers.delete(teamId);
}
export function receiveTeamTokenRates(teamId: string, event: MessageEvent) {
  try {
    const batch: unknown = JSON.parse(event.data);
    if (!isRecord(batch) || batch.teamId !== teamId || !isRecord(batch.rates))
      return;
    const entries = Object.entries(batch.rates);
    if (entries.length > 1024 || entries.some(([id]) => id.length > 200))
      return;
    const next = new Set(entries.map(([id]) => id));
    const previous = teamMembers.get(teamId);
    if (previous)
      for (const id of previous)
        if (!next.has(id)) publishRate(workerRateKey(teamId, id), null);
    teamMembers.set(teamId, next);
    for (const [id, value] of entries)
      publishRate(workerRateKey(teamId, id), value as TokenRate | null);
  } catch {
    /* Ignore malformed telemetry without affecting the team. */
  }
}
export function receiveTokenRate(id: string, event: MessageEvent) {
  try {
    const value: unknown = JSON.parse(event.data);
    publishRate(id, value as TokenRate | null);
  } catch {
    /* A malformed telemetry event does not affect the transcript. */
  }
}
type WorkspaceRates = {
  rates: Record<string, TokenRate | null>;
  teams: Record<string, Record<string, TokenRate | null>>;
};
let workspaceRates: WorkspaceRates = { rates: {}, teams: {} };
const openTeams = new Map<string, number>();
function validRates(value: unknown): value is Record<string, TokenRate | null> {
  return Boolean(
    value &&
    typeof value === "object" &&
    !Array.isArray(value) &&
    Object.keys(value).length <= 1024 &&
    Object.entries(value).every(([id]) => id.length <= 200),
  );
}
function publishTeam(teamId: string) {
  receiveTeamTokenRates(teamId, {
    data: JSON.stringify({ teamId, rates: workspaceRates.teams[teamId] || {} }),
  } as MessageEvent);
}
// Uses the shared workspace stream. No transport or database belongs to a card.
export function watchTeamTokenRates(teamId: string) {
  const stopStream =
    typeof window !== "undefined" && watchStream
      ? watchStream(receiveResourceTokenRates)
      : () => {};
  openTeams.set(teamId, (openTeams.get(teamId) || 0) + 1);
  publishTeam(teamId);
  return () => {
    stopStream();
    const count = (openTeams.get(teamId) || 1) - 1;
    if (count) openTeams.set(teamId, count);
    else {
      openTeams.delete(teamId);
      clearTeamTokenRates(teamId);
    }
  };
}
export function receiveResourceTokenRates(
  event: components["schemas"]["ResourceTokenRatesEvent"],
) {
  return receiveWorkspaceTokenRates({ rates: event.rates, teams: event.teams });
}
export function receiveWorkspaceTokenRates(value: WorkspaceRates) {
  if (
    !validRates(value?.rates) ||
    !value.teams ||
    typeof value.teams !== "object" ||
    Array.isArray(value.teams) ||
    Object.keys(value.teams).length > 1024 ||
    Object.entries(value.teams).some(
      ([id, rates]) => id.length > 200 || !validRates(rates),
    )
  )
    return false;
  for (const id of Object.keys(workspaceRates.rates))
    if (!(id in value.rates)) publishRate(id, null);
  workspaceRates = value;
  for (const [id, rate] of Object.entries(value.rates)) publishRate(id, rate);
  for (const teamId of openTeams.keys()) publishTeam(teamId);
  return true;
}
export function clearWorkspaceTokenRates() {
  receiveWorkspaceTokenRates({ rates: {}, teams: {} });
}
export function subscribeTokenRate(
  id: string,
  listener: (value: TokenRate | null) => void,
) {
  let subscribers = listeners.get(id);
  if (!subscribers) listeners.set(id, (subscribers = new Set()));
  subscribers.add(listener);
  listener(values.get(id) || null);
  return () => {
    subscribers!.delete(listener);
    // Stryker disable next-line ConditionalExpression, CallExpression: The false variant and call removal only retain an empty private Set, unobservable through the exported API; the true variant is killed by "subscriptions fan out once and unsubscribe independently".
    if (!subscribers!.size) listeners.delete(id);
  };
}
export function getTokenRate(id: string) {
  return values.get(id) || null;
}
export function getWorkspaceTeamTokenRate(teamId: string, agentId: string) {
  return workspaceRates.teams[teamId]?.[agentId] || null;
}
export function tweenTokenRate(from: number, to: number, elapsed: number) {
  const progress = Math.min(1, Math.max(0, elapsed / 450));
  return from + (to - from) * (1 - (1 - progress) ** 3);
}
const standardRate = new Intl.NumberFormat(undefined, {
  maximumFractionDigits: 0,
  useGrouping: false,
});
const compactRate = new Intl.NumberFormat(undefined, {
  // Stryker disable next-line StringLiteral: This mutation throws RangeError at module load; Vitest collects zero tests and reports Survived (static, testsCompleted 0), so no test in this file can report it killed. This is a tool limitation, not an equivalent mutant.
  notation: "compact",
  maximumFractionDigits: 0,
  useGrouping: false,
});
export function formatTokenRate(value: number) {
  if (value > 0 && value < 1) return "<1";
  return (value >= 10000 ? compactRate : standardRate).format(value);
}
