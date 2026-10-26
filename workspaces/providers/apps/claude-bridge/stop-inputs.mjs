import {
  cancelNativeSessionInput,
  nativeInputAttempt,
} from "./session-identity.ts";
// SDK 0.3.285 implements interrupt({cancelQueued:true}) in sdk.mjs, although
// sdk.d.ts still declares interrupt() without arguments. The CLI receipt uses
// native UUIDs; Studio settles only its original input identities.
export async function interruptInputQueue(session, active, beforeInterrupt) {
  const inputs = new Map();
  for (const turn of session.turns) {
    for (const item of turn.items) {
      if (item.type !== "userMessage" || !item.nativeId) continue;
      const identity =
        item.delivery === "steer"
          ? item.id
          : turn.clientUserMessageId || turn.id;
      inputs.set(identity, {
        item,
        attempt: nativeInputAttempt(turn, item),
        uuids: item.nativeIds || [item.nativeId],
      });
    }
  }
  // Withdraw before the first await: the SDK iterator must not refill the
  // native queue after the atomic interrupt has swept it.
  const withdrawn =
    typeof active.input.withdraw === "function"
      ? active.input.withdraw()
      : active.input.values.splice(0);
  const locallyCancelled = new Set();
  for (const input of withdrawn) {
    for (const part of input.studioInputBatch || [input]) {
      if (inputs.get(part.studioInputIdentity)?.uuids.includes(part.uuid)) {
        locallyCancelled.add(part.uuid);
      }
    }
  }
  const markCancelled = (identity, item) => {
    item.deliveryStatus = "cancelled";
    item.error = "Cancelled by Stop before Claude read this input";
    active.pendingSteers.delete(identity);
  };
  for (const [identity, { item, uuids }] of inputs) {
    if (uuids.every((uuid) => locallyCancelled.has(uuid))) {
      markCancelled(identity, item);
    }
  }
  if (beforeInterrupt) await beforeInterrupt();
  const native = await active.q?.interrupt({ cancelQueued: true });
  const cancelled = new Set([
    ...locallyCancelled,
    ...(native?.cancelled || []),
  ]);
  const survivors = new Set(native?.still_queued || []);
  const receipt = { cancelled: [], still_queued: [] };
  for (const [identity, { item, uuids, attempt }] of inputs) {
    // A partial split-input cancellation does not prove the input was unread.
    if (uuids.every((uuid) => cancelled.has(uuid) && !survivors.has(uuid))) {
      receipt.cancelled.push(identity);
      markCancelled(identity, item);
      cancelNativeSessionInput(session, attempt);
    } else if (
      uuids.some((uuid) => cancelled.has(uuid) || survivors.has(uuid)) ||
      (!native && item.deliveryStatus === "submitted")
    ) {
      receipt.still_queued.push(identity);
      item.deliveryStatus = "submitted";
      delete item.error;
    }
  }
  return receipt;
}
