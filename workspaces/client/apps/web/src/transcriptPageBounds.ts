import type { Message } from "./types";

export const MAX_TRANSCRIPT_PAGE_ITEMS = 240;
export const MAX_TRANSCRIPT_PAGE_BYTES = 4_000_000;
export const MAX_TRANSCRIPT_CACHE_ITEMS = 1_200;
export const MAX_TRANSCRIPT_CACHE_BYTES = 12_000_000;

export function boundTranscriptPage(
  items: Message[],
  sizeOf: (item: Message) => number,
  direction: "older" | "newer" | "around" | "latest",
  cursors: {
    before: string | null;
    after: string | null;
    nextBefore: string | null;
    nextAfter: string | null;
  },
  anchorId?: string,
) {
  const bounded = boundTranscriptItems(
    items,
    sizeOf,
    direction === "older" ? "oldest" : "newest",
    direction === "newer" ? undefined : anchorId,
  );
  let before = direction === "newer" ? cursors.before : cursors.nextBefore;
  let after = cursors.nextAfter;
  if (bounded.droppedOldest) before = bounded.items[0]?.id || before;
  if (direction === "older")
    after =
      bounded.droppedNewest || cursors.after
        ? bounded.items.at(-1)?.id || null
        : null;
  else if (direction === "newer") {
    if (cursors.nextAfter) after = bounded.items.at(-1)?.id || null;
    before =
      bounded.droppedOldest || cursors.before
        ? bounded.items[0]?.id || null
        : null;
  } else if (direction === "around" && bounded.droppedNewest)
    after = bounded.items.at(-1)?.id || null;
  return { ...bounded, before, after };
}

export function boundTranscriptItems(
  items: Message[],
  sizeOf: (item: Message) => number,
  keep: "oldest" | "newest" = "newest",
  anchorId?: string,
) {
  const result: Message[] = [];
  let bytes = 0;
  const anchorIndex = anchorId
    ? items.findIndex(
        (item) => item.id === anchorId || item.sourceId === anchorId,
      )
    : -1;
  const start =
    anchorIndex >= 0
      ? Math.max(
          0,
          Math.min(anchorIndex - 120, items.length - MAX_TRANSCRIPT_PAGE_ITEMS),
        )
      : keep === "oldest"
        ? 0
        : Math.max(0, items.length - MAX_TRANSCRIPT_PAGE_ITEMS);
  const source =
    anchorIndex >= 0
      ? items.slice(start)
      : keep === "oldest"
        ? items
        : [...items].reverse();
  for (const item of source) {
    const size = sizeOf(item);
    if (
      result.length >= MAX_TRANSCRIPT_PAGE_ITEMS ||
      bytes + size > MAX_TRANSCRIPT_PAGE_BYTES
    )
      break;
    result.push(item);
    bytes += size;
  }
  if (anchorIndex >= 0) {
    // The anchor-centered window keeps the visible row while pruning both sides.
  } else if (keep === "newest") result.reverse();
  const retained = new Set(result);
  const firstRetained = items.findIndex((item) => retained.has(item));
  return {
    items: result,
    bytes,
    droppedOldest: Math.max(0, firstRetained),
    droppedNewest: Math.max(
      0,
      items.length - (Math.max(0, firstRetained) + result.length),
    ),
  };
}

export function trimTranscriptPageCache<
  T extends { items: Message[]; size: number },
>(pages: Map<string, T>, keepScope: string) {
  let items = 0;
  let bytes = 0;
  for (const page of pages.values()) {
    items += page.items.length;
    bytes += page.size;
  }
  while (
    pages.size > 12 ||
    items > MAX_TRANSCRIPT_CACHE_ITEMS ||
    bytes > MAX_TRANSCRIPT_CACHE_BYTES
  ) {
    const oldest = [...pages.keys()].find((scope) => scope !== keepScope);
    if (!oldest) break;
    const page = pages.get(oldest)!;
    pages.delete(oldest);
    items -= page.items.length;
    bytes -= page.size;
  }
}
