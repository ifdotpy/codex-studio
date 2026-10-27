import type { Agent } from "../../src/types.ts";
import type { components } from "../../src/generated/api.ts";
import type { AddressInfo } from "node:net";
import type { ViteDevServer } from "vite";
export function fixtureOrigin(server: ViteDevServer) {
  return (
    "http://127.0.0.1:" + (server.httpServer!.address() as AddressInfo).port
  );
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

export function required<T>(value: T | null | undefined): T {
  if (value == null) throw new Error("Missing performance fixture value");
  return value;
}
