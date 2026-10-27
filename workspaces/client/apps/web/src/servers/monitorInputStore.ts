// Monitor input metadata is authoritative here, never in cached preferences.
export const MONITOR_INPUT_DATABASE_NAME = "studio-monitor-input-journal-v1";
const INPUT_STORE = "inputs";
export type MonitorInputEntry = {
  requestId: string;
  payloadDigest: string;
  uncertain: boolean;
};
export type MonitorInputStore = {
  transition<T>(
    monitorId: string,
    update: (current: MonitorInputEntry | undefined) => {
      entry: MonitorInputEntry | undefined;
      result: T;
    },
  ): Promise<T>;
};

export function validMonitorInput(
  value: unknown,
): MonitorInputEntry | undefined {
  if (value === undefined) return;
  if (value && typeof value === "object" && !Array.isArray(value)) {
    const entry = value as Record<string, unknown>;
    if (
      Object.keys(entry).length === 3 &&
      typeof entry.requestId === "string" &&
      !!entry.requestId &&
      typeof entry.payloadDigest === "string" &&
      /^[0-9a-f]{64}$/.test(entry.payloadDigest) &&
      typeof entry.uncertain === "boolean"
    )
      return entry as MonitorInputEntry;
  }
  throw new Error(
    "The monitor input journal is invalid. Do not send another input until its result is confirmed.",
  );
}

export function createMonitorInputStore(
  serverId: string | null,
): MonitorInputStore {
  const owner = serverId || "local";
  async function database(): Promise<IDBDatabase> {
    if (typeof indexedDB === "undefined") {
      throw new Error(
        "IndexedDB is unavailable. Monitor input cannot be saved safely.",
      );
    }
    return new Promise((resolve, reject) => {
      const request = indexedDB.open(MONITOR_INPUT_DATABASE_NAME, 1);
      let failed = false;
      request.onupgradeneeded = () =>
        request.result.createObjectStore(INPUT_STORE);
      request.onblocked = () => {
        failed = true;
        reject(
          new Error(
            "The monitor input journal is blocked. Close older Studio tabs and try again.",
          ),
        );
      };
      request.onerror = () =>
        reject(
          request.error ||
            new Error("The monitor input journal could not be opened."),
        );
      request.onsuccess = () => {
        const db = request.result;
        if (failed) {
          db.close();
          return;
        }
        db.onversionchange = () => db.close();
        resolve(db);
      };
    });
  }
  return {
    async transition<T>(
      monitorId: string,
      update: (current: MonitorInputEntry | undefined) => {
        entry: MonitorInputEntry | undefined;
        result: T;
      },
    ): Promise<T> {
      if (!monitorId) throw new Error("A monitor ID is required for input.");
      const db = await database();
      return new Promise<T>((resolve, reject) => {
        let tx: IDBTransaction;
        try {
          tx = db.transaction(INPUT_STORE, "readwrite", {
            durability: "strict",
          });
        } catch (error) {
          db.close();
          reject(error);
          return;
        }
        let result: T;
        let failure: unknown;
        const store = tx.objectStore(INPUT_STORE);
        const key = [owner, monitorId];
        const request = store.get(key);
        request.onsuccess = () => {
          try {
            const next = update(validMonitorInput(request.result));
            if (next.entry === undefined) store.delete(key);
            else store.put(validMonitorInput(next.entry), key);
            result = next.result;
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
              new Error("The monitor input journal transaction failed."),
          );
        };
      });
    },
  };
}
