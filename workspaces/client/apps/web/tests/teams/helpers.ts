import type { SyncEntity } from "../playwright.ts";
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
export function legacySyncEntities(
  response: unknown,
): SyncEntity[] | undefined {
  // Legacy fixture responses used these generated entity DTOs before their removal.
  const legacyResponse = response as { _syncEntities?: SyncEntity[] };
  return legacyResponse._syncEntities;
}

import type { ApiGetPath, GetResult } from "../../src/apiTypes.ts";

/** The prepared fixture server owns GET response shapes, generated from its runtime schema. */
export async function fixtureGetResult<Path extends ApiGetPath>(
  response: Pick<APIResponse | Response, "json">,
  _path: Path,
): Promise<GetResult<Path>> {
  return response.json();
}

export function required<T>(value: T | null | undefined): T {
  if (value === null || value === undefined)
    throw new Error("Missing fixture value");
  return value;
}
