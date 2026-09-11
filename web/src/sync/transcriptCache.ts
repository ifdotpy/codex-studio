import type { SyncDocument } from "./client";

export type CachedTranscript = { payload: any | null; seq: number };
const entries = new Map<string, { value: CachedTranscript; size: number }>();
const listeners = new Map<string, Set<(entry: CachedTranscript) => void>>();
const maxEntries = 32;
const maxSize = 24 * 1024 * 1024;
let size = 0;
const keyOf = (workspaceId: string, id: string) => `${workspaceId}:${id}`;

export function peekTranscript(workspaceId: string, id: string) {
  const key = keyOf(workspaceId, id);
  const entry = entries.get(key);
  if (entry) {
    entries.delete(key);
    entries.set(key, entry);
  }
  return entry?.value;
}

export function subscribeTranscript(
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
    if (!set.size) listeners.delete(key);
  };
}

/** Server sequence orders replies and tombstones within one workspace. */
export function cacheTranscript(
  workspaceId: string,
  id: string,
  doc: SyncDocument,
) {
  const key = keyOf(workspaceId, id);
  const previous = entries.get(key);
  if (previous && previous.value.seq >= doc.seq) return;
  const value: CachedTranscript = {
    payload: doc._deleted ? null : JSON.parse(doc.payload),
    seq: doc.seq,
  };
  const entrySize = doc._deleted ? 0 : doc.payload.length * 2;
  if (previous) size -= previous.size;
  entries.delete(key);
  entries.set(key, { value, size: entrySize });
  size += entrySize;
  while (entries.size > maxEntries || size > maxSize) {
    const oldest = entries.entries().next().value;
    if (!oldest) break;
    entries.delete(oldest[0]);
    size -= oldest[1].size;
  }
  listeners.get(key)?.forEach((accept) => accept(value));
}
