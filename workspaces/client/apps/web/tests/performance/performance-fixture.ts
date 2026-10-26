import type { Agent } from "../../src/types.ts";
import type { components } from "../../src/generated/api.ts";
import type { ViteDevServer } from "vite";
export function fixtureOrigin(server: ViteDevServer) {
  const address = server.httpServer?.address();
  if (!address || typeof address === "string")
    throw new Error("Missing fixture server port");
  return "http://127.0.0.1:" + address.port;
}
export function required<T>(value: T | null | undefined): T {
  if (value == null) throw new Error("Missing performance fixture value");
  return value;
}

export type PerformanceTranscriptItem = Pick<
  import("../playwright.ts").TranscriptPageResponse["items"][number],
  | "id"
  | "role"
  | "text"
  | "turnId"
  | "turnStatus"
  | "phase"
  | "toolStatus"
  | "title"
>;

export function transcriptFixtureAgent(
  agent: Agent,
): Agent & components["schemas"]["TranscriptAgent"] {
  // Seeded by this fixture; the seed never sets contextUsage. These agents arrive
  // through fixture documents whose broad owner type loses that seed invariant.
  return agent as Agent & components["schemas"]["TranscriptAgent"];
}
