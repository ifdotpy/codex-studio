// This cache receives volatile SSE events. It never enters the sync database.
export type TokenRate = {
  turnId: string;
  active: boolean;
  estimated: boolean;
  rate: number;
  outputTokens: number;
};
const values = new Map<string, TokenRate | null>();
const listeners = new Map<string, Set<(value: TokenRate | null) => void>>();
const teamMembers = new Map<string, Set<string>>();
function validRate(value: TokenRate | null) {
  return (
    value === null ||
    (typeof value.turnId === "string" &&
      typeof value.active === "boolean" &&
      typeof value.estimated === "boolean" &&
      Number.isFinite(value.rate) &&
      value.rate >= 0 &&
      Number.isFinite(value.outputTokens) &&
      value.outputTokens >= 0)
  );
}
function publishRate(id: string, value: TokenRate | null) {
  values.delete(id);
  values.set(id, value);
  while (values.size > 2048) values.delete(values.keys().next().value!);
  for (const listener of listeners.get(id) || []) listener(value);
}
export const workerRateKey = (teamId: string, agentId: string) =>
  `team:${teamId}:${agentId}`;
export function clearTeamTokenRates(teamId: string) {
  for (const id of teamMembers.get(teamId) || []) {
    const key = workerRateKey(teamId, id);
    publishRate(key, null);
    values.delete(key);
  }
  teamMembers.delete(teamId);
}
export function receiveTeamTokenRates(teamId: string, event: MessageEvent) {
  try {
    const batch = JSON.parse(event.data);
    if (
      batch.teamId !== teamId ||
      !batch.rates ||
      typeof batch.rates !== "object" ||
      Array.isArray(batch.rates)
    )
      return;
    const entries = Object.entries(batch.rates) as [string, TokenRate | null][];
    if (
      entries.length > 1024 ||
      entries.some(([id, value]) => id.length > 200 || !validRate(value))
    )
      return;
    const next = new Set(entries.map(([id]) => id));
    for (const id of teamMembers.get(teamId) || [])
      if (!next.has(id)) publishRate(workerRateKey(teamId, id), null);
    teamMembers.set(teamId, next);
    for (const [id, value] of entries)
      publishRate(workerRateKey(teamId, id), value);
  } catch {
    /* Ignore malformed telemetry without affecting the team. */
  }
}
export function receiveTokenRate(id: string, event: MessageEvent) {
  try {
    const value: TokenRate | null = JSON.parse(event.data);
    if (!validRate(value)) return;
    publishRate(id, value);
  } catch {
    /* A malformed telemetry event does not affect the transcript. */
  }
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
    if (!subscribers!.size) listeners.delete(id);
  };
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
  notation: "compact",
  maximumFractionDigits: 0,
  useGrouping: false,
});
export function formatTokenRate(value: number) {
  if (value > 0 && value < 1) return "<1";
  return (value >= 10000 ? compactRate : standardRate).format(value);
}
