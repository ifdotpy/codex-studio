import type {
  ApiGetPath,
  ApiPostPath,
  GetResult,
  PostBody,
  PostResult,
} from "../../src/apiTypes.ts";
import type { GetOptions } from "../../src/api.ts";
async function get<Path extends ApiGetPath>(
  path: Path,
  options?: GetOptions<Path>,
): Promise<GetResult<Path>> {
  const response = await fetch(
    path + "?" + new URLSearchParams(options?.query as Record<string, string>),
  );
  if (!response.ok) throw new Error("Read failed");
  return response.json();
}
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
const api = {
  get,
  post,
  apiDownload: async () => ({
    blob: new Blob(["failed check"]),
    name: "gate.log",
    truncated: false,
  }),
};
export const useServerApi = () => api;
export const useServerStorage = () => localStorage;
