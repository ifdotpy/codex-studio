import type { PostBody, PostResult } from "./api";

type RenameBody = PostBody<"/api/rename">;
type RenameReceipt = PostResult<"/api/rename">;

export async function submitRename(
  body: RenameBody,
  post: (path: "/api/rename", body: RenameBody) => Promise<RenameReceipt>,
  recover: () => Promise<unknown>,
): Promise<RenameReceipt> {
  void recover;
  const result = await post("/api/rename", body);
  if (result.status === "pending") {
    window.setTimeout(() => void submitRename(body, post, recover), 1000);
  }
  return result;
}
