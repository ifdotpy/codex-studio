// The one synchronous store for preference and UI state. The desktop app
// installs the Rust core store at startup (native/preferences.ts). Until then,
// and in a plain browser, values stay in memory and are lost on reload. The page
// never writes browser storage for them.
export function memoryStorage(): Storage {
  const values = new Map<string, string>();
  return {
    get length() {
      return values.size;
    },
    key: (index) => [...values.keys()][index] ?? null,
    getItem: (key) => values.get(String(key)) ?? null,
    setItem: (key, value) => {
      values.set(String(key), String(value));
    },
    removeItem: (key) => {
      values.delete(String(key));
    },
    clear: () => values.clear(),
  };
}

let installed: Storage = memoryStorage();

export function installPreferenceStorage(storage: Storage) {
  installed = storage;
}

export function preferenceStorage(): Storage {
  return installed;
}
