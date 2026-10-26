import type { components } from "../generated/api";
import { afterEach, expect, it, vi } from "vitest";
import { createSessionCostCache } from "./sessionCostCache";

function storage(): Storage {
  const values = new Map<string, string>();
  return {
    get length() {
      return values.size;
    },
    key: (index) => [...values.keys()][index] ?? null,
    getItem: (key) => values.get(key) ?? null,
    setItem: (key, value) => {
      values.set(key, value);
    },
    removeItem: (key) => {
      values.delete(key);
    },
    clear: () => values.clear(),
  };
}
afterEach(() => vi.restoreAllMocks());
it("isolates in-memory and persisted estimates for matching session IDs", () => {
  const storageA = storage(),
    storageB = storage();
  const a = createSessionCostCache(storageA),
    b = createSessionCostCache(storageB);
  const cost = (totalUSD: number) =>
    ({
      rootId: "same",
      cacheAgeSeconds: 0,
      pricedSamples: 0,
      refreshing: false,
      pricingState: "ready",
      estimated: true,
      unknownModels: [],
      breakdown: { models: {}, providers: {} },
      totalUSD,
    }) satisfies components["schemas"]["SessionCostResponse"];
  a.storeSessionCost("state", "same", cost(1));
  expect(b.peekSessionCost("state", "same")).toBeNull();
  b.storeSessionCost("state", "same", cost(9));
  expect(a.peekSessionCost("state", "same")?.totalUSD).toBe(1);
  expect(
    createSessionCostCache(storageB).peekSessionCost("state", "same")?.totalUSD,
  ).toBe(9);
});
