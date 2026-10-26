import type {
  ApiGetPath,
  ApiPostPath,
  GetResult,
  PostBody,
  PostResult,
} from "../../src/apiTypes.ts";

// These fixture boundaries receive responses from Studio and requests from its
// renderer under test, both governed by the generated route contract.
export function fixturePostBody<Path extends ApiPostPath>(
  _path: Path,
  body: unknown,
): PostBody<Path> {
  return body as PostBody<Path>;
}
export async function fixtureGetResult<Path extends ApiGetPath>(
  _path: Path,
  response: Pick<Response, "json">,
): Promise<GetResult<Path>> {
  return (await response.json()) as GetResult<Path>;
}
export async function fixturePostResult<Path extends ApiPostPath>(
  _path: Path,
  response: Pick<Response, "json">,
): Promise<PostResult<Path>> {
  return (await response.json()) as PostResult<Path>;
}

// Resource selectors are sent by the renderer to its fixture stream URL.
export function fixtureResourceRefs(
  value: string,
): import("../playwright.ts").ResourceRef[] {
  return JSON.parse(value) as import("../playwright.ts").ResourceRef[];
}
