import { createFrameBatch } from "./frameBatch";

import type { TranscriptPageData } from "../types";
type TranscriptPayload = Omit<Partial<TranscriptPageData>, "items"> &
  Pick<TranscriptPageData, "items">;
type TranscriptItem = TranscriptPageData["items"][number];
export type CachedTranscript = {
  payload: TranscriptPayload | null;
  seq: number;
};
export function createTranscriptCache() {
  const entries = new Map<
    string,
    { value: CachedTranscript; size: number; itemIndexes: Map<string, number> }
  >();

  const listeners = new Map<string, Set<(entry: CachedTranscript) => void>>();

  const pendingNotifications = new Map<string, CachedTranscript>();

  const publication = createFrameBatch(() => {
    const pending = [...pendingNotifications];
    pendingNotifications.clear();
    for (const [key, value] of pending)
      listeners.get(key)?.forEach((accept) => accept(value));
  });

  function notify(key: string, value: CachedTranscript) {
    if (!listeners.has(key)) return;
    pendingNotifications.set(key, value);
    publication.schedule();
  }

  const maxEntries = 256;

  const maxSize = 64 * 1024 * 1024;

  let size = 0;

  const keyOf = (workspaceId: string, id: string) => `${workspaceId}:${id}`;

  function peekTranscript(workspaceId: string, id: string) {
    const key = keyOf(workspaceId, id);
    const entry = entries.get(key);
    if (entry) {
      entries.delete(key);
      entries.set(key, entry);
    }
    return entry?.value;
  }

  /** Drop a memory-only transcript when a successful pull confirms no local row. */
  function clearTranscript(workspaceId: string, id: string) {
    const key = keyOf(workspaceId, id);
    const previous = entries.get(key);
    if (previous) size -= previous.size;
    entries.delete(key);
    pendingNotifications.delete(key);
  }

  function subscribeTranscript(
    workspaceId: string,
    id: string,
    accept: (entry: CachedTranscript) => void,
  ) {
    const key = keyOf(workspaceId, id);
    let set = listeners.get(key);
    if (!set) listeners.set(key, (set = new Set()));
    set.add(accept);
    const entry = peekTranscript(workspaceId, id);
    if (entry) accept(entry);
    return () => {
      set.delete(accept);
      if (!set.size) {
        listeners.delete(key);
        pendingNotifications.delete(key);
      }
    };
  }

  /** Cache a decoded transcript without serializing and parsing the page again. */
  function cacheTranscriptValue(
    workspaceId: string,
    id: string,
    payload: TranscriptPayload | null,
    seq: number,
    sizeHint?: number,
  ) {
    const key = keyOf(workspaceId, id);
    const previous = entries.get(key);
    if (previous && previous.value.seq >= seq) return;
    const value: CachedTranscript = {
      payload,
      seq,
    };
    const itemIndexes = new Map<string, number>();
    for (const [index, item] of (payload?.items || []).entries())
      if (typeof item?.id === "string") itemIndexes.set(item.id, index);
    const entrySize = sizeHint ?? JSON.stringify(payload).length * 2;
    if (previous) size -= previous.size;
    entries.delete(key);
    entries.set(key, { value, size: entrySize, itemIndexes });
    size += entrySize;
    while (entries.size > maxEntries || size > maxSize) {
      const oldest = entries.entries().next().value;
      if (!oldest) break;
      entries.delete(oldest[0]);
      size -= oldest[1].size;
    }
    notify(key, value);
  }

  /** Apply a sparse server delta while preserving previously published snapshots. */
  function patchTranscriptValue(
    workspaceId: string,
    id: string,
    payload: Omit<Partial<TranscriptPageData>, "items">,
    seq: number,
    changed: TranscriptPageData["items"],
    removed: string[],
    order?: string[],
  ) {
    const key = keyOf(workspaceId, id);
    const entry = entries.get(key);
    if (!entry || entry.value.seq >= seq || !entry.value.payload) return false;
    const oldSize = entry.size;
    // Hooks retain published payloads between frame-batched notifications and
    // for history navigation. Patch a private array so removals cannot expose
    // null tombstones there; unchanged message objects keep their identities.
    const oldItems: (TranscriptItem | null)[] = [...entry.value.payload.items];
    const indexes = entry.itemIndexes;
    for (const item of changed) {
      if (typeof item?.id !== "string") continue;
      const previousIndex = indexes.get(item.id);
      if (previousIndex === undefined) {
        indexes.set(item.id, oldItems.length);
        oldItems.push(item);
        entry.size += JSON.stringify(item).length * 2;
      } else {
        const previous = oldItems[previousIndex];
        entry.size +=
          (JSON.stringify(item).length - JSON.stringify(previous).length) * 2;
        oldItems[previousIndex] = item;
      }
    }
    for (const itemId of removed) {
      const index = indexes.get(itemId);
      if (index === undefined) continue;
      entry.size -= JSON.stringify(oldItems[index]).length * 2;
      indexes.delete(itemId);
      oldItems[index] = null;
    }
    let items: (TranscriptItem | null)[] = oldItems;
    if (order) {
      items = order
        .map((itemId) => {
          const index = indexes.get(itemId);
          return index === undefined ? undefined : oldItems[index];
        })
        .filter(
          (item): item is TranscriptItem => item !== null && item !== undefined,
        );
    } else if (removed.length) {
      items = oldItems.filter(
        (item): item is TranscriptItem => item !== null && item !== undefined,
      );
    }
    if (items !== oldItems || removed.length) {
      indexes.clear();
      items.forEach((item, index) => {
        if (item) indexes.set(item.id, index);
      });
    }
    entry.value = {
      seq,
      payload: {
        ...entry.value.payload,
        ...payload,
        items: items.filter((item): item is TranscriptItem => item !== null),
      },
    };
    if (entry.size < 0) entry.size = 0;
    size += entry.size - oldSize;
    while (entries.size > maxEntries || size > maxSize) {
      const oldest = entries.entries().next().value;
      if (!oldest) break;
      entries.delete(oldest[0]);
      size -= oldest[1].size;
    }
    notify(key, entry.value);
    return true;
  }
  return {
    peekTranscript,
    clearTranscript,
    subscribeTranscript,
    cacheTranscriptValue,
    patchTranscriptValue,
  };
}
export const {
  peekTranscript,
  clearTranscript,
  subscribeTranscript,
  cacheTranscriptValue,
  patchTranscriptValue,
} = createTranscriptCache();
