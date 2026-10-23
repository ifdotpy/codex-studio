import type { components } from "../generated/api";

import {
  TOKEN_RATE_MAX_ENTRIES,
  TOKEN_RATE_IDENTIFIER_MAX_CODEPOINTS,
} from "../generated/api";

// This cache receives volatile SSE events. It never enters the sync database.
export type TokenRate = components["schemas"]["TokenRateRecord"];
type TokenRateEvent = components["schemas"]["ResourceTokenRatesEvent"];
type WorkspaceRates = components["schemas"]["TokenRateSnapshot"];

export const workerRateKey = (teamId: string, agentId: string) =>
  `team:${teamId}:${agentId}`;

const recordKeys = {
  agentId: true,
  rootId: true,
  turnId: true,
  active: true,
  estimated: true,
  rate: true,
  outputTokens: true,
} satisfies Record<keyof TokenRate, true>;

function validIdentifier(value: unknown): value is string {
  if (typeof value !== "string" || !value.length) return false;
  let codepoints = 0;
  for (const _character of value) {
    if (++codepoints > TOKEN_RATE_IDENTIFIER_MAX_CODEPOINTS) return false;
  }
  return true;
}

// One validation at the untrusted snapshot boundary, using generated types and limits.
function isTokenRateSnapshot(value: unknown): value is WorkspaceRates {
  if (
    !value ||
    typeof value !== "object" ||
    !("records" in value) ||
    !Array.isArray(value.records) ||
    value.records.length > TOKEN_RATE_MAX_ENTRIES
  )
    return false;
  const agents = new Set<string>();
  return value.records.every((record: unknown): record is TokenRate => {
    if (!record || typeof record !== "object" || Array.isArray(record))
      return false;
    if (
      !("agentId" in record) ||
      !validIdentifier(record.agentId) ||
      agents.has(record.agentId)
    )
      return false;
    agents.add(record.agentId);
    return (
      "rootId" in record &&
      validIdentifier(record.rootId) &&
      "turnId" in record &&
      validIdentifier(record.turnId) &&
      "active" in record &&
      typeof record.active === "boolean" &&
      "estimated" in record &&
      typeof record.estimated === "boolean" &&
      "rate" in record &&
      typeof record.rate === "number" &&
      Number.isFinite(record.rate) &&
      record.rate >= 0 &&
      "outputTokens" in record &&
      typeof record.outputTokens === "number" &&
      Number.isFinite(record.outputTokens) &&
      record.outputTokens >= 0 &&
      Object.keys(record).every((key) => Object.hasOwn(recordKeys, key))
    );
  });
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
export function createTokenRateCache() {
  let watchStream:
    | ((listener: (event: TokenRateEvent) => void) => () => void)
    | undefined;

  function configureTokenRateStream(
    watch: (listener: (event: TokenRateEvent) => void) => () => void,
  ) {
    watchStream = watch;
  }

  // The coordinator applies each batch once. Interest requests its cached replay.
  function watchTokenRateInterest() {
    return typeof window !== "undefined" && watchStream
      ? watchStream(() => {})
      : () => {};
  }

  let records = new Map<string, TokenRate>();
  // A derived index contains identities only; every value belongs to records.
  let teamAgents = new Map<string, string>();
  const listeners = new Map<string, Set<(value: TokenRate | null) => void>>();
  const openTeams = new Map<string, number>();

  function notify(id: string) {
    const value = getTokenRate(id);
    for (const listener of listeners.get(id) || []) listener(value);
  }

  function notifyTeam(teamId: string) {
    for (const record of records.values())
      if (record.rootId === teamId)
        notify(workerRateKey(teamId, record.agentId));
  }

  function watchTeamTokenRates(teamId: string) {
    const stopStream = watchTokenRateInterest();
    openTeams.set(teamId, (openTeams.get(teamId) || 0) + 1);
    notifyTeam(teamId);
    return () => {
      stopStream();
      const count = (openTeams.get(teamId) || 1) - 1;
      if (count) openTeams.set(teamId, count);
      else {
        openTeams.delete(teamId);
        notifyTeam(teamId);
      }
    };
  }

  function receiveResourceTokenRates(event: TokenRateEvent) {
    return receiveWorkspaceTokenRates(event);
  }

  function receiveWorkspaceTokenRates(value: unknown) {
    if (!isTokenRateSnapshot(value)) return false;
    records = new Map(value.records.map((record) => [record.agentId, record]));
    teamAgents = new Map(
      value.records.map((record) => [
        workerRateKey(record.rootId, record.agentId),
        record.agentId,
      ]),
    );
    // Replacing the full snapshot also clears removed agents and old root memberships.
    for (const id of listeners.keys()) notify(id);
    return true;
  }

  function clearWorkspaceTokenRates() {
    receiveWorkspaceTokenRates({ records: [] });
  }

  function subscribeTokenRate(
    id: string,
    listener: (value: TokenRate | null) => void,
  ) {
    let subscribers = listeners.get(id);
    if (!subscribers) listeners.set(id, (subscribers = new Set()));
    subscribers.add(listener);
    listener(getTokenRate(id));
    return () => {
      subscribers.delete(listener);
      if (!subscribers.size) listeners.delete(id);
    };
  }

  function getTokenRate(id: string): TokenRate | null {
    const direct = records.get(id);
    if (direct) return direct;
    const agentId = teamAgents.get(id);
    const record = agentId ? records.get(agentId) : undefined;
    return record && openTeams.has(record.rootId) ? record : null;
  }

  function getWorkspaceTeamTokenRate(
    teamId: string,
    agentId: string,
  ): TokenRate | null {
    const record = records.get(agentId);
    return record?.rootId === teamId ? record : null;
  }
  return {
    configureTokenRateStream,
    watchTokenRateInterest,
    watchTeamTokenRates,
    receiveResourceTokenRates,
    receiveWorkspaceTokenRates,
    clearWorkspaceTokenRates,
    subscribeTokenRate,
    getTokenRate,
    getWorkspaceTeamTokenRate,
  };
}
export const {
  configureTokenRateStream,
  watchTokenRateInterest,
  watchTeamTokenRates,
  receiveResourceTokenRates,
  receiveWorkspaceTokenRates,
  clearWorkspaceTokenRates,
  subscribeTokenRate,
  getTokenRate,
  getWorkspaceTeamTokenRate,
} = createTokenRateCache();
