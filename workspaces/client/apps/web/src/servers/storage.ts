import {
  storageKeys,
  storageKeySnapshot,
  type IndexedStorage,
} from "../storageKeys";
import { serverViewId } from "./environment";

type StorageWriteObserver = (
  key: string,
  value: string | null,
  previous: string | null,
  storage: Storage,
) => void;
let writeObserver: StorageWriteObserver | undefined;
export function observeStorageWrites(observer: StorageWriteObserver) {
  writeObserver = observer;
}

export function scopedStorage(
  storage: () => Storage,
  prefix: string,
  shared = new Set<string>(),
): Storage {
  let sourceKeys: readonly string[] | undefined;
  let selectedKeys: readonly string[] = [];
  let sharedKeys: string[] = [];
  const keys = () => {
    const all = storageKeys(storage());
    if (!prefix) return all;
    if (
      all !== sourceKeys ||
      sharedKeys.length !== shared.size ||
      sharedKeys.some((key) => !shared.has(key))
    ) {
      sourceKeys = all;
      sharedKeys = [...shared];
      selectedKeys = Object.freeze(
        all.filter((key) => key.startsWith(prefix) || shared.has(key)),
      );
    }
    return selectedKeys;
  };
  const result: IndexedStorage = {
    [storageKeySnapshot]() {
      // Expose logical keys for nested adapters, with a stable snapshot below.
      const selected = keys();
      if (selected !== logicalSource) {
        logicalSource = selected;
        logicalKeys = Object.freeze(
          selected.map((key) =>
            shared.has(key) ? key : key.slice(prefix.length),
          ),
        );
      }
      return logicalKeys;
    },
    get length() {
      return prefix ? keys().length : storage().length;
    },
    key(index) {
      if (!prefix) return storage().key(index);
      const key = keys()[index];
      return key === undefined
        ? null
        : shared.has(key)
          ? key
          : key.slice(prefix.length);
    },
    getItem(key) {
      return storage().getItem(shared.has(key) ? key : prefix + key);
    },
    setItem(key, value) {
      const previous = this.getItem(key);
      storage().setItem(shared.has(key) ? key : prefix + key, value);
      writeObserver?.(key, value, previous, this);
    },
    removeItem(key) {
      const previous = this.getItem(key);
      storage().removeItem(shared.has(key) ? key : prefix + key);
      writeObserver?.(key, null, previous, this);
    },
    clear() {
      if (prefix) keys().forEach((key) => storage().removeItem(key));
      else storage().clear();
    },
  };
  let logicalSource: readonly string[] | undefined;
  let logicalKeys: readonly string[] = [];
  return result;
}
// Prefix for browser records owned by one server; local records keep their old keys.
export const serverStoragePrefix = (owner: string | null) =>
  owner && owner !== "local" ? `:server:${owner}:` : "";
const prefix = serverStoragePrefix(serverViewId);
export const serverLocalStorage = scopedStorage(
  () => globalThis.localStorage,
  prefix,
  new Set(["codex-studio-preferences-v1", "mantine-color-scheme-value"]),
);
export const serverSessionStorage = scopedStorage(
  () => globalThis.sessionStorage,
  prefix,
);
export function createStorageEventKey(prefix: string) {
  return (event: Pick<StorageEvent, "key">) =>
    event.key === null
      ? null
      : event.key.startsWith(prefix)
        ? event.key.slice(prefix.length)
        : undefined;
}
export const serverStorageEventKey = createStorageEventKey(prefix);
