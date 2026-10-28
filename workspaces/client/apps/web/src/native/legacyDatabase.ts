// Reads a browser store that an older build left behind. It never creates one.
export async function openLegacyDatabase(
  name: string,
): Promise<IDBDatabase | null> {
  const saved = (await indexedDB.databases()).find((db) => db.name === name);
  if (!saved) return null;
  return new Promise((resolve, reject) => {
    const request = indexedDB.open(name, saved.version);
    request.onupgradeneeded = () => request.transaction?.abort();
    request.onerror = () => reject(request.error);
    request.onsuccess = () => resolve(request.result);
  });
}

export function readLegacyStore<T>(
  db: IDBDatabase,
  store: string,
  action: (objectStore: IDBObjectStore) => IDBRequest<T>,
): Promise<T> {
  return new Promise((resolve, reject) => {
    const transaction = db.transaction(store, "readonly");
    const request = action(transaction.objectStore(store));
    transaction.oncomplete = () => resolve(request.result);
    transaction.onabort = transaction.onerror = () =>
      reject(
        transaction.error ||
          request.error ||
          new Error("The saved browser data could not be read."),
      );
  });
}
