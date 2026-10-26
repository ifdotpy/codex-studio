import type { Query } from "@anthropic-ai/claude-agent-sdk";
import type { Session, Item } from "./session-state.ts";
import type { StudioInputMessage } from "./input-messages.ts";
import type { ActiveQuery } from "./bridge.ts";
export type StopActive = Pick<ActiveQuery, "pendingSteers"> & {
  input: Pick<ActiveQuery["input"], "values"> &
    Partial<Pick<ActiveQuery["input"], "withdraw">>;
  q?: {
    interrupt(options?: { cancelQueued: true }): ReturnType<Query["interrupt"]>;
  } | null;
};

import {
  cancelNativeSessionInput,
  nativeInputAttempt,
} from "./session-identity.ts";
// SDK 0.3.285 implements interrupt({cancelQueued:true}) in sdk.mjs, although
// sdk.d.ts still declares interrupt() without arguments. The CLI receipt uses
// native UUIDs; Studio settles only its original input identities.
export async function interruptInputQueue(
  session: Session,
  active: StopActive,
  beforeInterrupt?: () => unknown | Promise<unknown>,
) {
  const inputs = new Map<
    string | undefined,
    {
      item: Item;
      attempt: ReturnType<typeof nativeInputAttempt>;
      uuids: string[];
    }
  >();
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
  const locallyCancelled = new Set<StudioInputMessage["uuid"]>();
  for (const input of withdrawn) {
    for (const part of input.studioInputBatch || [input]) {
      if (inputs.get(part.studioInputIdentity)?.uuids.includes(part.uuid)) {
        locallyCancelled.add(part.uuid);
      }
    }
  }
  const markCancelled = (identity: string | undefined, item: Item) => {
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
  const receipt: {
    cancelled: (string | undefined)[];
    still_queued: (string | undefined)[];
  } = { cancelled: [], still_queued: [] };
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
