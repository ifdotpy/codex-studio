// Storage adapters may expose a stable key snapshot. Its identity changes only
// when membership/order changes; values remain live through getItem.
export const storageKeySnapshot = Symbol("storageKeySnapshot");
export type IndexedStorage = Storage & {
  [storageKeySnapshot]?: () => readonly string[];
};
export function storageKeys(storage: Storage): readonly string[] {
  const indexed = storage as IndexedStorage;
  if (indexed[storageKeySnapshot]) return indexed[storageKeySnapshot]();
  const keys: string[] = [];
  for (let index = 0; index < storage.length; index++) {
    const key = storage.key(index);
    if (key !== null) keys.push(key);
  }
  return keys;
}
