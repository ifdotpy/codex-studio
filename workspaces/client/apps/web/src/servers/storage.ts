import {
  storageKeys,
  storageKeySnapshot,
  type IndexedStorage,
} from "../storageKeys";
import { serverViewId } from "./environment";
import { preferenceCacheKey, preferenceScope } from "../sync/uiPreferenceMerge";

const preferenceValues = new Map<string, string>();
let preferenceKeys: readonly string[] = Object.freeze([]);
export const preferenceMemory: IndexedStorage = {
  [storageKeySnapshot]: () => preferenceKeys,
  get length() {
    return preferenceValues.size;
  },
  key: (index) => preferenceKeys[index] ?? null,
  getItem: (key) => preferenceValues.get(key) ?? null,
  setItem(key, value) {
    const added = !preferenceValues.has(key);
    preferenceValues.set(key, value);
    if (added) preferenceKeys = Object.freeze([...preferenceValues.keys()]);
  },
  removeItem(key) {
    if (preferenceValues.delete(key))
      preferenceKeys = Object.freeze([...preferenceValues.keys()]);
  },
  clear() {
    preferenceValues.clear();
    preferenceKeys = Object.freeze([]);
  },
};

function isPreferenceKey(key: string) {
  return (
    !!preferenceScope(key) ||
    key.startsWith(`${preferenceCacheKey}:`) ||
    key === "studio-ui-preference-writer-v1" ||
    key === "mantine-color-scheme-value"
  );
}

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
  let sourcePreferenceKeys: readonly string[] | undefined;
  let selectedKeys: readonly string[] = [];
  let sharedKeys: string[] = [];
  const keys = () => {
    const diskKeys = storageKeys(storage());
    const memoryKeys = storageKeys(preferenceMemory);
    if (
      diskKeys !== sourceKeys ||
      memoryKeys !== sourcePreferenceKeys ||
      sharedKeys.length !== shared.size ||
      sharedKeys.some((key) => !shared.has(key))
    ) {
      sourceKeys = diskKeys;
      sourcePreferenceKeys = memoryKeys;
      sharedKeys = [...shared];
      selectedKeys = Object.freeze(
        [
          ...diskKeys.filter((key) => {
            const logical = key.startsWith(prefix)
              ? key.slice(prefix.length)
              : key;
            return !isPreferenceKey(logical);
          }),
          ...memoryKeys,
        ].filter((key) => key.startsWith(prefix) || shared.has(key)),
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
      return keys().length;
    },
    key(index) {
      const key = keys()[index];
      return key === undefined
        ? null
        : shared.has(key)
          ? key
          : key.slice(prefix.length);
    },
    getItem(key) {
      // Temporary routing until request fences no longer use scoped storage.
      return (isPreferenceKey(key) ? preferenceMemory : storage()).getItem(
        shared.has(key) ? key : prefix + key,
      );
    },
    setItem(key, value) {
      const previous = this.getItem(key);
      (isPreferenceKey(key) ? preferenceMemory : storage()).setItem(
        shared.has(key) ? key : prefix + key,
        value,
      );
      writeObserver?.(key, value, previous, this);
    },
    removeItem(key) {
      const previous = this.getItem(key);
      (isPreferenceKey(key) ? preferenceMemory : storage()).removeItem(
        shared.has(key) ? key : prefix + key,
      );
      writeObserver?.(key, null, previous, this);
    },
    clear() {
      if (prefix)
        keys().forEach((key) => {
          const logical = shared.has(key) ? key : key.slice(prefix.length);
          (isPreferenceKey(logical) ? preferenceMemory : storage()).removeItem(
            key,
          );
        });
      else {
        storage().clear();
        preferenceMemory.clear();
      }
    },
  };
  let logicalSource: readonly string[] | undefined;
  let logicalKeys: readonly string[] = [];
  return result;
}
// Prefix for browser records owned by one server; local records keep their old keys.
export const serverStoragePrefix = (owner: string | null) =>
  owner && owner !== "local" ? `:server:${owner}:` : "";
const localStore = () => globalThis.localStorage;
export function preferenceStorageForServer(id: string) {
  return scopedStorage(
    localStore,
    serverStoragePrefix(id),
    new Set(["codex-studio-preferences-v1", "mantine-color-scheme-value"]),
  );
}
const prefix = serverStoragePrefix(serverViewId);
export const serverLocalStorage = scopedStorage(
  localStore,
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
