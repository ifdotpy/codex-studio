import {
  useServerSync,
  useServerOutbox,
} from "../servers/ServerServicesContext";
import { useEffect, useRef, useState } from "react";
import "../api";
import "../sync/send";
import {
  latestMessageReceipt,
  type MessageReceipt,
} from "../sync/messageReceipts";

export function useMessageReceipts(
  room: string | null,
  scope: string,
  workspaceId: string | undefined,
  ids: string[],
) {
  const { subscribeStateProjection } = useServerSync();
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
    let queued = Promise.resolve();
    let previousSource: MessageReceipt[] | undefined;
    let selected: MessageReceipt[] | undefined;
    const stop = subscribeStateProjection(
      (snapshot) => {
        if (!snapshot) return;
        const source = snapshot.runtime.messageReceipts;
        if (source === previousSource) return;
        previousSource = source;
        const keep = new Set(requested.current);
        const receipts = (source ?? []).filter(
          (receipt) => receipt.agent === room && keep.has(receipt.id),
        );
        if (
          selected?.length === receipts.length &&
          receipts.every((receipt, index) => receipt === selected?.[index])
        )
          return;
        selected = receipts;
        queued = queued
          .then(async () => {
            if (!active || controller.signal.aborted) return;
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
          })
          .catch(() => {
            // A later projection can retry receipts that could not be persisted.
            previousSource = undefined;
            selected = undefined;
          });
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
  }, [
    room,
    scope,
    workspaceId,
    requestedKey,
    subscribeStateProjection,
    reconcileOutboxReceipts,
  ]);
  return state.scope === scope
    ? state.receipts
    : new Map<string, MessageReceipt>();
}
