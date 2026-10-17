import {
  useServerApi,
  useServerOutbox,
} from "../servers/ServerServicesContext";
import { useEffect, useRef, useState } from "react";
import "../api";
import "../sync/send";
import {
  checkedMessageReceipts,
  latestMessageReceipt,
  type MessageReceipt,
} from "../sync/messageReceipts";
import { useServerResourceReads } from "../servers/ServerServicesContext";

export function useMessageReceipts(
  room: string | null,
  scope: string,
  workspaceId: string | undefined,
  ids: string[],
) {
  const watchResourceReads = useServerResourceReads();
  const { get } = useServerApi();
  const { reconcileOutboxReceipts } = useServerOutbox();

  const requested = useRef(ids);
  requested.current = ids;
  const requestedKey = JSON.stringify(ids);
  const [state, setState] = useState({
    scope,
    receipts: new Map<string, MessageReceipt>(),
  });
  useEffect(() => {
    if (!room || !workspaceId) return;
    let active = true;
    const controller = new AbortController();
    const stop = watchResourceReads(
      { kind: "receipts", agentId: room },
      async () => {
        const current = requested.current;
        for (
          let offset = 0;
          active && !controller.signal.aborted && offset < current.length;
          offset += 100
        ) {
          const batch = current.slice(offset, offset + 100);
          if (!batch.length) continue;
          const result = await get("/api/messages/receipts", {
            query: { agent: room, ids: JSON.stringify(batch) },
            workspaceId,
            signal: controller.signal,
          });
          if (!active || controller.signal.aborted) return;
          const receipts = checkedMessageReceipts(
            result,
            room,
            batch,
            workspaceId,
          );
          // Publishing delivery can retire the local message and change the
          // requested IDs, which aborts this effect. Persist the receipt first.
          await reconcileOutboxReceipts(
            room,
            receipts,
            workspaceId,
            controller.signal,
          );
          if (!active || controller.signal.aborted) return;
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
        }
      },
      () => {
        // Preserve known receipts until a successful read confirms delivery.
      },
    );
    return () => {
      active = false;
      controller.abort();
      stop();
    };
  }, [room, scope, workspaceId, requestedKey, watchResourceReads]);
  return state.scope === scope
    ? state.receipts
    : new Map<string, MessageReceipt>();
}
