import { useEffect, useRef, useState } from "react";
import { get } from "../api";
import { reconcileOutboxReceipts } from "../sync/send";
import {
  checkedMessageReceipts,
  latestMessageReceipt,
  type MessageReceipt,
} from "../sync/messageReceipts";
import { watchResourceReads } from "./watchResourceReads";

export function useMessageReceipts(
  room: string | null,
  scope: string,
  workspaceId: string | undefined,
  ids: string[],
) {
  const requested = useRef(ids);
  requested.current = ids;
  const requestedKey = JSON.stringify(ids);
  const [state, setState] = useState({
    scope,
    receipts: new Map<string, MessageReceipt>(),
  });
  useEffect(() => {
    if (!room) return;
    let active = true;
    const stop = watchResourceReads(
      { kind: "receipts", agentId: room },
      async () => {
        try {
          const current = requested.current;
          for (let offset = 0; offset < current.length; offset += 100) {
            const batch = current.slice(offset, offset + 100);
            if (!batch.length) continue;
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
              // Keep confirmed receipts while retained history still has an old
              // pending record. Bound this cache to the caller's current IDs.
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
          }
        } catch {
          // Missing or unavailable receipts cannot prove delivery. Preserve
          // the known receipt until a later resource event permits another read.
        }
      },
      () => {},
    );
    return () => {
      active = false;
      stop();
    };
  }, [room, scope, workspaceId, requestedKey]);
  return state.scope === scope
    ? state.receipts
    : new Map<string, MessageReceipt>();
}
