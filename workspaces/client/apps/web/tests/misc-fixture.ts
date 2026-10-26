import type { ViteDevServer } from "vite";
import type { GetResult, PostBody, PostResult } from "../src/api.ts";
import type { ApiGetPath, ApiPostPath } from "../src/apiTypes.ts";

export function fixtureOrigin(server: ViteDevServer): string {
  const url = server.resolvedUrls?.local[0];
  if (!url) throw new Error("Fixture server has no local URL");
  return url.replace(/\/$/, "");
}

// Requests originate in the renderer under test and use the generated API contract.
export function fixturePostBody<Path extends ApiPostPath>(
  body: unknown,
): PostBody<Path> {
  return body as PostBody<Path>;
}

// Fixture HTTP replies use the generated route contract, like the production client boundary.
export function fixtureGetResult<Path extends ApiGetPath>(
  body: unknown,
): GetResult<Path> {
  return body as GetResult<Path>;
}

// A fixture owns the response it publishes for this route.
export function fixturePostResult<Path extends ApiPostPath>(
  body: unknown,
): PostResult<Path> {
  return body as PostResult<Path>;
}
