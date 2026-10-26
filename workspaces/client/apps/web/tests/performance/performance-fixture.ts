import type { Agent } from "../../src/types.ts";
import type { components } from "../../src/generated/api.ts";
import type { ApiPostPath, PostBody } from "../../src/apiTypes.ts";
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

export function fixturePostBody<Path extends ApiPostPath>(
  value: unknown,
): PostBody<Path> {
  // Fixture callers are the typed renderer under test.
  return value as PostBody<Path>;
}

export function transcriptFixtureAgent(agent: Agent) {
  const { contextUsage, ...rest } = agent;
  // Fresh fixture seeds do not inject context usage. Preserve that absence; any
  // future usage seed must include the timestamp required by transcript responses.
  if (contextUsage == null)
    return {
      ...rest,
      contextUsage,
    } satisfies components["schemas"]["TranscriptAgent"];
  if (typeof contextUsage.at !== "number")
    throw new Error("Transcript fixture context usage needs a timestamp");
  return {
    ...rest,
    contextUsage: { ...contextUsage, at: contextUsage.at },
  } satisfies components["schemas"]["TranscriptAgent"];
}
