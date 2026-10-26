/** Require a fixture member or measured element before using it. */
export function required<T>(value: T | null | undefined): T {
  if (value === null || value === undefined)
    throw new Error("Missing fixture value");
  return value;
}

import type { Route, APIResponse } from "@playwright/test";
import type { ApiPostPath, PostBody, PostResult } from "../../src/apiTypes.ts";

/** Requests are written by the renderer under test using its generated API contract. */
export function fixturePostBody<Path extends ApiPostPath>(
  route: Route,
  _path: Path,
): PostBody<Path> {
  return route.request().postDataJSON();
}

/** Responses come from the prepared Studio fixture server, whose route owns this shape. */
export async function fixturePostResult<Path extends ApiPostPath>(
  response: Pick<APIResponse | Response, "json">,
  _path: Path,
): Promise<PostResult<Path>> {
  return response.json();
}

/** Preserve old mutation-extension fixtures; current Studio responses omit this field. */
export function legacySyncEntities(response: unknown): unknown[] | undefined {
  if (
    !response ||
    typeof response !== "object" ||
    !("_syncEntities" in response)
  )
    return undefined;
  return Array.isArray(response._syncEntities)
    ? response._syncEntities
    : undefined;
}
