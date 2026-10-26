export function fixtureRoot(): HTMLElement {
  const root = document.getElementById("root");
  if (!root) throw new Error("Fixture root is missing");
  return root;
}

export function usageSessionCost() {
  return {
    pricingState: "ready",
    totalUSD: 0,
    rootId: "chat",
    claudeHistoryIncomplete: true,
    breakdown: { providers: {}, models: {} },
    cacheAgeSeconds: 0,
    estimated: false,
    pricedSamples: 0,
    refreshing: false,
    unknownModels: [],
  } satisfies import("../../src/generated/api.ts").components["schemas"]["SessionCostResponse"];
}

export function usageAccountCost(accountKey: string) {
  return {
    accountKey,
    at: null,
    error: null,
    refreshing: false,
    stale: false,
    data: { todayUSD: 0, last30DaysUSD: 0 },
  } satisfies import("../../src/generated/api.ts").components["schemas"]["AccountCostResponse"];
}
