import type { components } from "../generated/api";
export type SessionCostValue = Partial<
  components["schemas"]["SessionCostResponse"]
>;
type Entry = { scope: string; value: SessionCostValue; storedAt: number };

const maxEntries = 16;
export const sessionCostScope = (stateDir: string, rootId: string) =>
  JSON.stringify([stateDir, rootId]);

export function createSessionCostCache() {
  const entries = new Map<string, Entry>();
  function remember(entry: Entry) {
    entries.delete(entry.scope);
    entries.set(entry.scope, entry);
    while (entries.size > maxEntries)
      entries.delete(entries.keys().next().value!);
    return entry;
  }

  function peekSessionCost(
    stateDir: string,
    rootId: string,
  ): SessionCostValue | null {
    const scope = sessionCostScope(stateDir, rootId);
    const entry = entries.get(scope);
    if (!entry) return null;
    remember(entry);
    const age = Math.max(0, (Date.now() - entry.storedAt) / 1000);
    return {
      ...entry.value,
      cacheAgeSeconds: Math.max(Number(entry.value.cacheAgeSeconds) || 0, age),
      refreshing: true,
    };
  }

  function storeSessionCost(
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
    return entry.value;
  }

  return { peekSessionCost, storeSessionCost };
}
export const { peekSessionCost, storeSessionCost } = createSessionCostCache();
