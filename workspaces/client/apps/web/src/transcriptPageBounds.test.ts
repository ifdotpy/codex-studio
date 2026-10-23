import assert from "node:assert/strict";
import { it } from "vitest";
import {
  boundTranscriptPage,
  updateLiveTranscriptPage,
} from "./transcriptPageBounds.ts";
import type { Message } from "./types.ts";

const item = (index: number): Message => ({
  id: `m${index}`,
  at: index,
  role: "assistant" as const,
  text: "message",
});
const sizeOf = (message: Message) => message.text.length * 2;
const merge = (first: Message[], second: Message[]) =>
  [
    ...new Map(
      [...first, ...second].map((message) => [message.id, message]),
    ).values(),
  ].sort(
    (a, b) => Number(a.at ?? a.created ?? 0) - Number(b.at ?? b.created ?? 0),
  );

it("recovers the history cursor when a short cache precedes a disjoint truncated page", () => {
  const page = {
    items: [item(0)],
    before: null as string | null,
    focused: false,
  };
  updateLiveTranscriptPage(page, [item(0), item(1)], { truncated: false });
  const fresh = Array.from({ length: 120 }, (_, index) => item(index + 120));
  updateLiveTranscriptPage(page, fresh, {
    truncated: true,
    nextCursor: "m120",
  });
  assert.equal(page.before, "m120");
  assert.deepEqual(page.items, fresh);
});

it("keeps a complete contiguous retained range without inventing an older cursor", () => {
  const page = { items: [item(0), item(1)], before: null, focused: false };
  updateLiveTranscriptPage(page, [item(1), item(2)], {
    truncated: true,
    nextCursor: "m1",
  });
  assert.equal(page.before, null);
  assert.deepEqual(page.items, [item(0), item(1), item(2)]);
});

it("preserves an existing older cursor when the live range overlaps", () => {
  const page = { items: [item(1), item(2)], before: "m1", focused: false };
  updateLiveTranscriptPage(page, [item(2), item(3)], {
    truncated: true,
    nextCursor: "m2",
  });
  assert.equal(page.before, "m1");
  assert.deepEqual(page.items, [item(1), item(2), item(3)]);
});

it("keeps a focused history page when the fresh range is disjoint", () => {
  const page = { items: [item(1), item(2)], before: "m1", focused: true };
  updateLiveTranscriptPage(page, [item(120)], {
    truncated: true,
    nextCursor: "m120",
  });
  assert.equal(page.before, "m1");
  assert.deepEqual(page.items, [item(1), item(2)]);
});

it.each([{ pending: true }, { materialized: false }])(
  "does not use an optimistic row as evidence of continuity: %j",
  (optimistic) => {
    const page = {
      items: [item(0), { ...item(120), ...optimistic }],
      before: null as string | null,
      focused: false,
    };
    const fresh = [item(120), item(121)];
    updateLiveTranscriptPage(page, fresh, {
      truncated: true,
      nextCursor: "m120",
    });
    assert.equal(page.before, "m120");
    assert.deepEqual(page.items, fresh);
  },
);

it.each([{ pending: true }, { materialized: false }])(
  "does not use an incoming optimistic row as evidence of continuity: %j",
  (optimistic) => {
    const page = {
      items: [item(0), item(1)],
      before: null as string | null,
      focused: false,
    };
    const fresh = [{ ...item(1), ...optimistic }, item(120), item(121)];
    updateLiveTranscriptPage(page, fresh, {
      truncated: true,
      nextCursor: "m120",
    });
    assert.equal(page.before, "m120");
    assert.deepEqual(page.items, fresh);
  },
);

it("keeps both paging cursors on the held window boundary", () => {
  const all = Array.from({ length: 301 }, (_, index) => item(index));
  const earlier = boundTranscriptPage(all, sizeOf, "older", {
    before: null,
    after: null,
    nextBefore: null,
    nextAfter: "m60",
  });
  assert.equal(earlier.items[0]?.id, "m0");
  assert.equal(earlier.items.at(-1)?.id, "m239");
  assert.equal(earlier.after, "m239");
  assert.equal(
    earlier.after,
    earlier.items.at(-1)?.id,
    "the first forward request starts after the held edge rather than an old server cursor",
  );
  const heldIds = new Set(earlier.items.map((message) => message.id));
  const firstForwardResponse = all.slice(
    all.findIndex((message) => message.id === earlier.after) + 1,
  );
  assert.equal(firstForwardResponse[0]?.id, "m240");
  assert.equal(
    firstForwardResponse.some((message) => heldIds.has(message.id)),
    false,
    "the first forward response contains rows beyond the held edge, not duplicates",
  );

  const forward = boundTranscriptPage(
    merge(earlier.items, firstForwardResponse),
    sizeOf,
    "newer",
    {
      before: earlier.before,
      after: earlier.after,
      nextBefore: null,
      nextAfter: null,
    },
    "m120",
  );
  assert.equal(forward.items[0]?.id, "m61");
  assert.equal(forward.items.at(-1)?.id, "m300");
  assert.equal(forward.after, null, "an exhausted page hides Later messages");
  assert.equal(
    forward.before,
    forward.items[0]?.id,
    "the older cursor follows the first row retained after the forward bound",
  );
  assert.ok(forward.items.length <= 240);
  assert.ok(forward.bytes <= 4_000_000);
});

it("only offers Later when the held window omits newer rows", () => {
  const all = Array.from({ length: 250 }, (_, index) => item(index));
  const page = boundTranscriptPage(
    merge(all.slice(30, 130), all.slice(130)),
    sizeOf,
    "older",
    {
      before: null,
      after: null,
      nextBefore: "m30",
      nextAfter: "m129",
    },
  );
  assert.equal(page.items.length, 220);
  assert.equal(page.after, null);

  const newestOnly = Array.from({ length: 100 }, (_, index) => item(index));
  const newerWithoutOldHistory = boundTranscriptPage(
    newestOnly,
    sizeOf,
    "newer",
    {
      before: null,
      after: "m99",
      nextBefore: null,
      nextAfter: null,
    },
  );
  assert.equal(newerWithoutOldHistory.items.length, 100);
  assert.equal(newerWithoutOldHistory.before, null);

  const olderAndClipped = boundTranscriptPage(
    Array.from({ length: 260 }, (_, index) => item(index)),
    sizeOf,
    "older",
    {
      before: null,
      after: null,
      nextBefore: null,
      nextAfter: "m259",
    },
  );
  assert.equal(olderAndClipped.items.at(-1)?.id, "m239");
  assert.equal(olderAndClipped.after, "m239");
});

it("keeps focused-around forward cursors on a locally clipped edge", () => {
  const all = Array.from({ length: 301 }, (_, index) => item(index));
  const around = boundTranscriptPage(
    all,
    sizeOf,
    "around",
    {
      before: null,
      after: null,
      nextBefore: "m0",
      nextAfter: "m300",
    },
    "m150",
  );
  assert.equal(around.items[0]?.id, "m30");
  assert.equal(around.items.at(-1)?.id, "m269");
  assert.equal(around.before, "m30");
  assert.equal(around.after, "m269");
});

it("aligns the byte-bounded forward page cursors with its retained edges", () => {
  const all = Array.from({ length: 40 }, (_, index) => item(index));
  const byteBounded = boundTranscriptPage(all, () => 200_000, "newer", {
    before: "m0",
    after: "m19",
    nextBefore: null,
    nextAfter: "m39",
  });
  assert.equal(byteBounded.items.length, 20);
  assert.equal(byteBounded.items[0]?.id, "m20");
  assert.equal(byteBounded.items.at(-1)?.id, "m39");
  assert.equal(byteBounded.before, "m20");
  assert.equal(byteBounded.after, "m39");
  assert.equal(byteBounded.bytes, 4_000_000);
});

it("advances the held window through several forward pages", () => {
  const all = Array.from({ length: 701 }, (_, index) => item(index));
  let page = boundTranscriptPage(all.slice(-240), sizeOf, "latest", {
    before: null,
    after: null,
    nextBefore: "m461",
    nextAfter: null,
  });
  for (let cursor = page.before; cursor; cursor = page.before) {
    const end = all.findIndex((message) => message.id === cursor);
    const start = Math.max(0, end - 120);
    const response = all.slice(start, end);
    const last = end - 1;
    page = boundTranscriptPage(merge(response, page.items), sizeOf, "older", {
      before: page.before,
      after: page.after,
      nextBefore: start > 0 ? response[0]?.id || null : null,
      nextAfter: last < all.length - 1 ? response.at(-1)?.id || null : null,
    });
    assert.equal(
      page.after,
      page.items.at(-1)?.id,
      "backward paging keeps the cursor at the held window's newest row",
    );
  }
  assert.equal(page.items[0]?.id, "m0");
  assert.equal(page.items.at(-1)?.id, "m239");
  assert.equal(page.after, "m239");

  for (let boundary = 239; boundary < 700;) {
    const start = boundary + 1;
    const response = all.slice(start, start + 100);
    const last = start + response.length - 1;
    const nextAfter = last < all.length - 1 ? `m${last}` : null;
    page = boundTranscriptPage(merge(page.items, response), sizeOf, "newer", {
      before: page.before,
      after: page.after,
      nextBefore: null,
      nextAfter,
    });
    assert.equal(page.items.at(-1)?.id, `m${last}`);
    assert.equal(page.after, nextAfter ? `m${last}` : null);
    boundary = last;
  }
  assert.equal(page.items.at(-1)?.id, "m700");
  assert.equal(page.after, null);
});
