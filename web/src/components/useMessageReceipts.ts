import { useEffect, useRef, useState } from "react";
import { get } from "../api";
import { onResume } from "../sync/resume";
import { reconcileOutboxReceipts } from "../sync/send";
import {
  checkedMessageReceipts,
  latestMessageReceipt,
  type MessageReceipt,
} from "../sync/messageReceipts";

export function useMessageReceipts(
  room: string | null,
  scope: string,
  workspaceId: string | undefined,
  ids: string[],
) {
  const requested = useRef(ids);
  requested.current = ids;
  const [state, setState] = useState({
    scope,
    receipts: new Map<string, MessageReceipt>(),
  });
  useEffect(() => {
    if (!room) return;
    let active = true,
      busy = false,
      offset = 0;
    let timer: ReturnType<typeof setTimeout>;
    const poll = async () => {
      if (!active || busy) return;
      clearTimeout(timer);
      if (document.hidden || navigator.onLine === false) return;
      const current = requested.current;
      if (offset >= current.length) offset = 0;
      const batch = current.slice(offset, offset + 100);
      offset += batch.length;
      busy = true;
      try {
        if (!batch.length) return;
        const result = await get("/api/messages/receipts", {
          query: { agent: room, ids: JSON.stringify(batch) },
          workspaceId,
        });
        if (!active) return;
        const receipts = checkedMessageReceipts(result, room, batch);
        setState((previous) => {
          const next = new Map(
            previous.scope === scope ? previous.receipts : [],
          );
          for (const receipt of receipts)
            next.set(
              receipt.id,
              latestMessageReceipt(next.get(receipt.id), receipt),
            );
          // Keep confirmed receipts while the retained history still has an old
          // pending record. Bound this cache to messages requested by the caller.
          const keep = new Set(requested.current);
          for (const id of next.keys()) if (!keep.has(id)) next.delete(id);
          if (
            previous.scope === scope &&
            next.size === previous.receipts.size &&
            [...next].every(
              ([id, receipt]) =>
                JSON.stringify(receipt) ===
                JSON.stringify(previous.receipts.get(id)),
            )
          )
            return previous;
          return { scope, receipts: next };
        });
        await reconcileOutboxReceipts(room, receipts, workspaceId);
      } catch {
        // Missing, unavailable, or older servers cannot prove delivery. Keep
        // the existing status and let the next bounded read try again.
      } finally {
        busy = false;
        if (active) timer = setTimeout(poll, 3000);
      }
    };
    void poll();
    const stop = onResume(() => void poll());
    return () => {
      active = false;
      clearTimeout(timer);
      stop();
    };
  }, [room, scope, workspaceId]);
  return state.scope === scope
    ? state.receipts
    : new Map<string, MessageReceipt>();
}
