import assert from "node:assert/strict";
import { it } from "vitest";
import { boundTranscriptPage } from "./transcriptPageBounds.ts";
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

  const duplicateOnly = boundTranscriptPage(
    merge(earlier.items, all.slice(60, 160)),
    sizeOf,
    "newer",
    {
      before: earlier.before,
      after: earlier.after,
      nextBefore: null,
      nextAfter: "m159",
    },
    "m120",
  );
  assert.equal(duplicateOnly.items[0]?.id, "m0");
  assert.equal(duplicateOnly.items.at(-1)?.id, "m239");
  assert.equal(duplicateOnly.after, "m239");

  const forward = boundTranscriptPage(
    merge(duplicateOnly.items, all.slice(240)),
    sizeOf,
    "newer",
    {
      before: duplicateOnly.before,
      after: duplicateOnly.after,
      nextBefore: null,
      nextAfter: null,
    },
    "m120",
  );
  assert.equal(forward.items.at(-1)?.id, "m300");
  assert.equal(forward.after, null, "an exhausted page hides Later messages");
  assert.ok(forward.items.length <= 240);
  assert.ok(forward.bytes <= 4_000_000);
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
