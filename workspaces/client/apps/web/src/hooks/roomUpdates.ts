import type { GetResult } from "../api";
import { agentChatMessages } from "./agentChatMessages";
import type { Message } from "../types";

type RoomResponse = GetResult<"/api/agent-chat">;

export async function drainRoomUpdates(
  current: Message[],
  checkpoint: number,
  read: (after: number) => Promise<RoomResponse>,
  isCurrent: () => boolean,
): Promise<{ items: Message[]; checkpoint: number } | null> {
  let after = checkpoint;
  let items = current;
  while (isCurrent()) {
    const result = await read(after);
    if (!isCurrent()) return null;
    const next = agentChatMessages(result.messages);
    if (!next.length) {
      if (result.nextAfter != null)
        throw new Error("The room page cursor advanced without messages.");
      break;
    }
    const nextCursor = next.at(-1)?.seq;
    if (typeof nextCursor !== "number" || nextCursor <= after)
      throw new Error("The room update cursor did not advance.");
    if (result.nextAfter != null && result.nextAfter !== nextCursor)
      throw new Error("The room page cursor did not match its final message.");
    items = [
      ...new Map(
        [...items, ...next].map((message) => [message.id, message]),
      ).values(),
    ].sort((left, right) => Number(left.seq ?? 0) - Number(right.seq ?? 0));
    after = result.nextAfter ?? nextCursor;
    if (result.nextAfter == null) break;
  }
  return isCurrent() ? { items, checkpoint: after } : null;
}
