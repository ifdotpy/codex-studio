export type FixtureStep = {
  item?: string;
  say?: string;
  extra?: Record<string, unknown>;
  delta?: string;
  append?: boolean;
  tool?: string;
  arguments?: Record<string, unknown>;
  notify?: { method: string; params: Record<string, unknown> };
  request?: { method: string; params: Record<string, unknown> };
  fail?: string;
  hold?: boolean;
  complete?: boolean;
};
export type FixtureScript = { prompt: string; steps: FixtureStep[] };

// Fixture scripts and model catalogs are written by the same test program.
export function parseFixtureScripts(text: string): FixtureScript[] {
  return JSON.parse(text);
}
export function parseFixtureAgentSteps(
  text: string,
): Record<string, FixtureStep[]> {
  return JSON.parse(text);
}
export function parseFixtureModels(
  text: string,
): import("../../src/generated/api.ts").components["schemas"]["ModelEntityDto"][] {
  return JSON.parse(text);
}

// Provider input has no generated Studio contract; narrow the JSON-RPC envelope.
export function readProviderObject(text: string): Record<string, unknown> {
  const value: unknown = JSON.parse(text);
  if (value === null || typeof value !== "object" || Array.isArray(value))
    throw new Error("Expected a fixture provider object");
  return value as Record<string, unknown>;
}
