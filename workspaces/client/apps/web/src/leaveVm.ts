import type { ApiPostPath, PostBody, PostOptions, PostResult } from "./api";

/** The request API of the server that owns the chat. */
type OwnerApi = {
  post<Path extends ApiPostPath>(
    path: Path,
    body: PostBody<Path>,
    options?: PostOptions,
  ): Promise<PostResult<Path>>;
  stableRequestId(key: string): string;
  clearStableRequestId(key: string): void;
};

/** Tooltip of the "Leave VM" menu item. */
export const leaveVmTitle = (serverName: string) =>
  `Move this chat from the virtual machine to ${serverName}`;

/**
 * Move a VM chat to host execution on the chat's own server. The caller passes
 * the API of that server (its view owns the chat). A retry after an uncertain
 * failure reuses the saved request ID, so the server never moves twice.
 */
export async function leaveVm(api: OwnerApi, id: string) {
  const requestId = api.stableRequestId(`leave-vm:${id}`);
  const result = await api.post(
    "/api/agents/move-to-host",
    { id, request_id: requestId },
    { timeoutMs: 3600000, requestId },
  );
  api.clearStableRequestId(`leave-vm:${id}`);
  return result;
}
