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
