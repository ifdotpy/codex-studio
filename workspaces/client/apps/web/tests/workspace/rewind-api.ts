import type { ApiPostPath, PostBody, PostResult } from "../../src/apiTypes.ts";
async function post<Path extends ApiPostPath>(
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
/** This fixture reads the same JSON preference values that the renderer wrote. */
function saved<T>(key: string, fallback: T): T {
  return (JSON.parse(localStorage.getItem(key) || "null") as T) || fallback;
}
const api = { post, saved };
export const useServerApi = () => api;
export const useServerStorage = () => localStorage;
