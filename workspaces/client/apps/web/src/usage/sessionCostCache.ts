import type { components } from "../generated/api";
import { nativeUsage, type UsagePersistence } from "./usageRequest";

export type SessionCostValue = Partial<
  components["schemas"]["SessionCostResponse"]
>;
type Entry = { scope: string; value: SessionCostValue; storedAt: number };

export const sessionCostScope = (stateDir: string, rootId: string) =>
  JSON.stringify([stateDir, rootId]);

// Memory holds the estimates shown now. The native core keeps the bounded copy for restarts.
export function createSessionCostCache(usage: UsagePersistence = nativeUsage) {
  const entries = new Map<string, Entry>();
  let writes: Promise<unknown> = Promise.resolve();
  const request = usage();
  if (request)
    request<Entry[]>("usage.sessionCosts.list", {}).then(
      (saved) => {
        for (const entry of saved)
          if (!entries.has(entry.scope)) entries.set(entry.scope, entry);
      },
      () => {
        /* This cache must not block the live estimate request. */
      },
    );

  function peekSessionCost(
    stateDir: string,
    rootId: string,
  ): SessionCostValue | null {
    const entry = entries.get(sessionCostScope(stateDir, rootId));
    if (!entry) return null;
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
    entries.set(scope, entry);
    const write = usage();
    if (write)
      writes = writes
        .then(() =>
          write<{ evicted: string[] }>("usage.sessionCosts.put", {
            scope,
            storedAt: entry.storedAt,
            value: entry.value,
          }),
        )
        .then(
          (result) => {
            // The core evicts by its own bounds; keep the entry this call stored.
            for (const evicted of result.evicted)
              if (evicted !== scope) entries.delete(evicted);
          },
          () => {
            /* Storage can be unavailable. The memory cache remains usable. */
          },
        );
    return entry.value;
  }

  return { peekSessionCost, storeSessionCost };
}
export const { peekSessionCost, storeSessionCost } = createSessionCostCache();
