export type SessionCostValue = Record<string, any>;
type Entry = { scope: string; value: SessionCostValue; storedAt: number };

const prefix = "studio-session-cost:";
const maxEntries = 16;
const maxStoredBytes = 64 * 1024;
const entries = new Map<string, Entry>();

export const sessionCostScope = (stateDir: string, rootId: string) =>
  JSON.stringify([stateDir, rootId]);

function valid(value: any, scope: string): value is Entry {
  return value?.scope === scope &&
    Number.isFinite(value?.storedAt) &&
    value?.value?.pricingState === "ready" &&
    value?.value?.estimated === true &&
    (value.value.totalUSD === null || Number.isFinite(value.value.totalUSD)) &&
    Array.isArray(value.value.unknownModels) &&
    value.value.breakdown !== null &&
    typeof value.value.breakdown === "object";
}

function remember(entry: Entry) {
  entries.delete(entry.scope);
  entries.set(entry.scope, entry);
  while (entries.size > maxEntries)
    entries.delete(entries.keys().next().value!);
  return entry;
}

export function peekSessionCost(
  stateDir: string,
  rootId: string,
): SessionCostValue | null {
  const scope = sessionCostScope(stateDir, rootId);
  let entry = entries.get(scope);
  if (!entry) {
    try {
      const saved = JSON.parse(localStorage.getItem(prefix + scope) || "null");
      if (valid(saved, scope)) entry = remember(saved);
      else if (saved !== null) localStorage.removeItem(prefix + scope);
    } catch {
      /* This cache must not block the live estimate request. */
    }
  }
  if (!entry) return null;
  remember(entry);
  const age = Math.max(0, (Date.now() - entry.storedAt) / 1000);
  return {
    ...entry.value,
    cacheAgeSeconds: Math.max(Number(entry.value.cacheAgeSeconds) || 0, age),
    refreshing: true,
  };
}

export function storeSessionCost(
  stateDir: string,
  rootId: string,
  value: SessionCostValue,
) {
  if (
    value?.pricingState !== "ready" ||
    value?.rootId !== rootId ||
    value?.estimated !== true ||
    !Array.isArray(value?.unknownModels) ||
    value?.breakdown === null ||
    typeof value?.breakdown !== "object"
  )
    return null;
  const scope = sessionCostScope(stateDir, rootId);
  const age = Math.max(0, Number(value.cacheAgeSeconds) || 0);
  const entry: Entry = {
    scope,
    value: { ...value },
    storedAt: Date.now() - age * 1000,
  };
  remember(entry);
  try {
    const key = prefix + scope;
    const serialized = JSON.stringify(entry);
    if (serialized.length * 2 > maxStoredBytes) {
      localStorage.removeItem(key);
      return entry.value;
    }
    const records: Array<{ key: string; bytes: number; at: number }> = [];
    for (let index = 0; index < localStorage.length; index++) {
      const candidate = localStorage.key(index);
      if (!candidate?.startsWith(prefix) || candidate === key) continue;
      const raw = localStorage.getItem(candidate) || "";
      let at = 0;
      try {
        at = Number(JSON.parse(raw).storedAt) || 0;
      } catch {
        /* Invalid cache records are evicted first. */
      }
      records.push({ key: candidate, bytes: raw.length * 2, at });
    }
    records.sort((a, b) => a.at - b.at);
    let bytes = serialized.length * 2 + records.reduce((sum, item) => sum + item.bytes, 0);
    while (records.length >= maxEntries || bytes > maxStoredBytes) {
      const oldest = records.shift();
      if (!oldest) break;
      localStorage.removeItem(oldest.key);
      bytes -= oldest.bytes;
    }
    localStorage.setItem(key, serialized);
  } catch {
    /* Storage can be full or unavailable. The memory cache remains usable. */
  }
  return entry.value;
}
