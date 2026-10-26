import type { Session, Item } from "./session-state.ts";
import { randomUUID } from "node:crypto";
const native = randomUUID();
const prefix = randomUUID();
import { test } from "vitest";
import assert from "node:assert/strict";
import { interruptInputQueue, type StopActive } from "./stop-inputs.ts";

function fixture(nativeIds: string[] = [native]) {
  const item: Item & { id: string } = {
    id: "studio:id",
    type: "userMessage",
    delivery: "steer",
    deliveryStatus: "submitted",
    nativeId: nativeIds.at(-1),
    nativeIds,
  };
  const session: Session = {
    id: "session",
    cwd: "/fixture",
    turns: [{ id: "turn", items: [item] }],
  };
  const active: StopActive = {
    input: { values: [] },
    pendingSteers: new Set([item.id]),
  };
  return { item, session, active };
}

test("Stop requires cancellation of every native part of a Studio input", async () => {
  for (const receipt of [
    { cancelled: [prefix, native], still_queued: [] },
    { cancelled: [native], still_queued: [] },
    { cancelled: [prefix, native], still_queued: [prefix] },
  ]) {
    const { item, session, active } = fixture([prefix, native]);
    active.q = {
      interrupt: async (options) => {
        assert.deepEqual(options, { cancelQueued: true });
        return receipt;
      },
    };
    const complete =
      receipt.cancelled.length === 2 && !receipt.still_queued.length;
    assert.deepEqual(await interruptInputQueue(session, active), {
      cancelled: complete ? [item.id] : [],
      still_queued: complete ? [] : [item.id],
    });
    assert.equal(item.deliveryStatus, complete ? "cancelled" : "submitted");
  }
});

test("Stop withdraws the unyielded batch tail before background task control", async () => {
  const { item, session, active } = fixture([prefix, native]);
  let withdrew = false;
  active.input.withdraw = () => {
    withdrew = true;
    return [{ uuid: native, studioInputIdentity: item.id }];
  };
  active.q = {
    interrupt: async () => ({
      cancelled: [prefix],
      still_queued: ["foreign"],
    }),
  };
  assert.deepEqual(
    await interruptInputQueue(session, active, async () => assert.ok(withdrew)),
    {
      cancelled: [item.id],
      still_queued: [],
    },
  );
});

test("an older CLI without a receipt cannot prove native input cancelled", async () => {
  const { item, session, active } = fixture();
  active.q = { interrupt: async () => undefined };
  assert.deepEqual(await interruptInputQueue(session, active), {
    cancelled: [],
    still_queued: [item.id],
  });
});
