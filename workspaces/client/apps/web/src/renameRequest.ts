import type { PostBody, PostResult } from "./api";

type RenameBody = PostBody<"/api/rename">;
type RenameReceipt = PostResult<"/api/rename">;

export async function submitRename(
  body: RenameBody,
  post: (path: "/api/rename", body: RenameBody) => Promise<RenameReceipt>,
  recover: () => Promise<unknown>,
): Promise<RenameReceipt> {
  try {
    // Success (including pending) is completed by the agent entity upsert.
    return await post("/api/rename", body);
  } catch (error) {
    // A lost write response permits one projection recovery, never another write.
    if (error instanceof TypeError) await recover().catch(() => {});
    throw error;
  }
}

/** The server replays this durable receipt inside the subscription baseline,
 * including after reconnect. A progress hint never starts an HTTP read. */
export function watchRenameResult(
  events: Pick<
    typeof import("./sync/subscriptions/events"),
    "watchResourceChanges"
  >,
  agentId: string,
  requestId: string,
  accept: (receipt: RenameReceipt) => void,
) {
  let active = true;
  const stop = events.watchResourceChanges(
    { kind: "rename-progress", agentId, requestId },
    (version) => {
      const progress = version?.progress;
      if (
        !active ||
        progress?.resource.kind !== "rename-progress" ||
        progress.resource.agentId !== agentId ||
        progress.resource.requestId !== requestId ||
        !progress.value
      )
        return;
      const receipt = (
        progress as Extract<
          import("./sync/subscriptions/events").ResourceProgress,
          { resource: { kind: "rename-progress" } }
        >
      ).value;
      if (
        !receipt ||
        receipt.id !== agentId ||
        receipt.request_id !== requestId ||
        receipt.status === "pending"
      )
        return;
      accept(receipt);
    },
  );
  return () => {
    active = false;
    stop();
  };
}
