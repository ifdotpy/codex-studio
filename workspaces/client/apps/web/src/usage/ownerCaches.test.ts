import type { components } from "../generated/api";
import { afterEach, expect, it, vi } from "vitest";
import { createSessionCostCache } from "./sessionCostCache";

afterEach(() => vi.restoreAllMocks());
it("isolates in-memory estimates without persisting them", () => {
  const a = createSessionCostCache(),
    b = createSessionCostCache();
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
  expect(createSessionCostCache().peekSessionCost("state", "same")).toBeNull();
});
