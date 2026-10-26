import type { Session, Item } from "./session-state.ts";
import { test } from "vitest";
import assert from "node:assert/strict";
import { interruptInputQueue, type StopActive } from "./stop-inputs.ts";

function fixture(nativeIds: string[] = ["native"]) {
  const item: Item & { id: string } = {
    id: "studio:id",
    type: "userMessage",
    delivery: "steer",
    deliveryStatus: "submitted",
    nativeId: nativeIds.at(-1),
    nativeIds,
  };
  // Cancellation reads only the mirror turns in this shared fixture.
  const session = {
    turns: [{ id: "turn", items: [item] }],
  } as Session;
  const active: StopActive = {
    input: { values: [] },
    pendingSteers: new Set([item.id]),
  };
  return { item, session, active };
}

test("Stop requires cancellation of every native part of a Studio input", async () => {
  for (const native of [
    { cancelled: ["prefix", "native"], still_queued: [] },
    { cancelled: ["native"], still_queued: [] },
    { cancelled: ["prefix", "native"], still_queued: ["prefix"] },
  ]) {
    const { item, session, active } = fixture(["prefix", "native"]);
    active.q = {
      interrupt: async (options) => {
        assert.deepEqual(options, { cancelQueued: true });
        return native;
      },
    };
    const complete =
      native.cancelled.length === 2 && !native.still_queued.length;
    assert.deepEqual(await interruptInputQueue(session, active), {
      cancelled: complete ? [item.id] : [],
      still_queued: complete ? [] : [item.id],
    });
    assert.equal(item.deliveryStatus, complete ? "cancelled" : "submitted");
  }
});

test("Stop withdraws the unyielded batch tail before background task control", async () => {
  const { item, session, active } = fixture(["prefix", "native"]);
  let withdrew = false;
  // @ts-expect-error The upstream fixture returns the descriptive "native" ID instead of a UUID.
  active.input.withdraw = () => {
    withdrew = true;
    return [
      {
        uuid: "native",
        studioInputIdentity: item.id,
      },
    ];
  };
  active.q = {
    interrupt: async () => ({
      cancelled: ["prefix"],
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

test("an older CLI without a native cannot prove native input cancelled", async () => {
  const { item, session, active } = fixture();
  active.q = { interrupt: async () => undefined };
  assert.deepEqual(await interruptInputQueue(session, active), {
    cancelled: [],
    still_queued: [item.id],
  });
});
