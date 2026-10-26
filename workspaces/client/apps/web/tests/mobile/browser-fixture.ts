import type { ViteDevServer } from "vite";

export function fixtureServerOrigin(server: ViteDevServer): string {
  const origin = server.resolvedUrls?.local[0];
  if (!origin) throw new Error("Fixture server did not publish a local URL");
  return origin;
}

export async function until(
  check: () => boolean | Promise<boolean>,
  message: string,
  timeoutMs = 15_000,
) {
  const deadline = Date.now() + timeoutMs;
  while (!(await check())) {
    if (Date.now() > deadline) throw new Error(message);
    await new Promise((resolve) => setTimeout(resolve, 20));
  }
}

import type { Request } from "@playwright/test";
import type { ApiPostPath, PostBody } from "../../src/apiTypes.ts";

/** These requests are written by the renderer under test using its generated API contract. */
export function fixturePostBody<Path extends ApiPostPath>(
  request: Request,
): PostBody<Path> {
  return request.postDataJSON();
}

import type { APIResponse } from "@playwright/test";
import type { PostResult } from "../../src/apiTypes.ts";

/** The fixture server returns the generated Studio response for this request. */
export async function fixturePostResult<Path extends ApiPostPath>(
  response: Pick<APIResponse, "json">,
): Promise<PostResult<Path>> {
  return response.json();
}
