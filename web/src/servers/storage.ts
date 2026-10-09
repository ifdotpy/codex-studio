import { serverStorageName } from "./environment";

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
  const keys = () => {
    const target = storage();
    return Array.from({ length: target.length }, (_, i) =>
      target.key(i)!,
    ).filter((key) => key.startsWith(prefix) || shared.has(key));
  };
  return {
    get length() {
      return prefix ? keys().length : storage().length;
    },
    key(index) {
      return prefix
        ? keys()[index]
          ? shared.has(keys()[index])
            ? keys()[index]
            : keys()[index].slice(prefix.length)
          : null
        : storage().key(index);
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
}
const namespace = serverStorageName("");
const prefix = namespace ? namespace + ":" : "";
export const serverLocalStorage = scopedStorage(
  () => globalThis.localStorage,
  prefix,
  new Set(["codex-studio-preferences-v1", "mantine-color-scheme-value"]),
);
export const serverSessionStorage = scopedStorage(
  () => globalThis.sessionStorage,
  prefix,
);
export function serverStorageEventKey(event: StorageEvent) {
  return event.key === null
    ? null
    : event.key.startsWith(prefix)
      ? event.key.slice(prefix.length)
      : undefined;
}
