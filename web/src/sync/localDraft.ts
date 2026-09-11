// Synchronous writes close the gap between an input event and page teardown.
// Keep the in-memory value when storage fails, and tell the caller to show it.
export function writeLocalDraft(key: string, value: unknown): string | null {
  try {
    if (value === null) localStorage.removeItem(key);
    else localStorage.setItem(key, JSON.stringify(value));
    return null;
  } catch {
    return "Changes could not be saved on this device. Keep this page open.";
  }
}

// IndexedDB serializes this read/write transaction across tabs, including Safari
// versions without Web Locks. The localStorage read and write share that lock.
export async function updateLocalDraft<T>(
  key: string,
  fallback: T,
  update: (current: T) => T,
): Promise<T> {
  const db = await new Promise<IDBDatabase>((resolve, reject) => {
    const request = indexedDB.open("codex-studio-draft-writes", 1);
    request.onupgradeneeded = () => request.result.createObjectStore("writes");
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error);
  });
  return new Promise((resolve, reject) => {
    const tx = db.transaction("writes", "readwrite");
    let result: T;
    let failure: unknown;
    tx.objectStore("writes").get(key).onsuccess = () => {
      try {
        const raw = localStorage.getItem(key);
        result = update(raw === null ? fallback : JSON.parse(raw));
        const error = writeLocalDraft(key, result);
        if (error) throw new Error(error);
      } catch (error) {
        failure = error;
        tx.abort();
      }
    };
    tx.oncomplete = () => {
      db.close();
      resolve(result!);
    };
    tx.onerror = tx.onabort = () => {
      db.close();
      reject(
        failure ||
          tx.error ||
          new Error("Draft changes could not be saved on this device."),
      );
    };
  });
}
