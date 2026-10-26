import type {
  ApiGetPath,
  ApiPostPath,
  GetResult,
  PostBody,
  PostResult,
} from "../../src/apiTypes.ts";
import type { GetOptions } from "../../src/api.ts";

/** Replies are authored by this fixture's route handlers using the generated contract. */
export async function get<Path extends ApiGetPath>(
  path: Path,
  options?: GetOptions<Path>,
): Promise<GetResult<Path>> {
  const query = new URLSearchParams();
  for (const [key, value] of Object.entries(options?.query ?? {})) {
    if (value !== undefined && value !== null) query.set(key, String(value));
  }
  const response = await fetch(path + "?" + query);
  if (!response.ok) throw new Error("Read failed");
  return response.json();
}
export async function post<Path extends ApiPostPath>(
  path: Path,
  body: PostBody<Path>,
): Promise<PostResult<Path>> {
  const response = await fetch(path, {
    method: "POST",
    body: JSON.stringify(body),
  });
  const result = await response.json();
  if (!response.ok) throw new Error(result.error);
  return result;
}
