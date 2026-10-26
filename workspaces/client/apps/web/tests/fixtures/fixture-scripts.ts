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

/** Indexed view of the fixture's RPC envelope; method handlers read their own fields. */
export type FixtureRpcMessage = {
  id: string;
  method: string;
  params?: { threadId?: string; turnId?: string; input?: unknown } & Record<
    string,
    unknown
  >;
};
export type FixtureControlInput = {
  id: string;
  thread: string;
  steps: FixtureStep[];
};
export type FixtureControlReply = { ok: boolean };

// The fixture RPC/control peers own these documents; handlers keep the base protocol behavior.
export function readProviderObject<
  T extends Record<string, unknown> = Record<string, unknown>,
>(text: string): T {
  return JSON.parse(text);
}
