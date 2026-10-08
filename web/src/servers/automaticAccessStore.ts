import type {
  AutomaticAccessAttempt,
  AutomaticAccessStore,
} from "./automaticUiAccess";
// Attempt identities contain no invitation secret. Keys and pair bodies stay in
// the existing browser credential store or desktop safeStorage.
export function attemptMetadata(
  value: AutomaticAccessAttempt,
): AutomaticAccessAttempt {
  const invitation = value.invitation;
  return {
    localServerId: value.localServerId,
    serverId: value.serverId,
    origin: value.origin,
    generation: value.generation,
    inviteRequestId: value.inviteRequestId,
    pairRequestId: value.pairRequestId,
    ...(value.pairStarted ? { pairStarted: true } : {}),
    ...(value.needsPairCheck ? { needsPairCheck: true as const } : {}),
    ...(invitation
      ? {
          invitation: {
            protocol: invitation.protocol,
            inviteId: invitation.inviteId,
            serverId: invitation.serverId,
            label: invitation.label,
            origin: invitation.origin,
            publicKey: invitation.publicKey,
            tailscaleUser: invitation.tailscaleUser,
            expires: invitation.expires,
          },
        }
      : {}),
  };
}
export function automaticAccessStore(
  hasPairAttempt: (attempt: AutomaticAccessAttempt) => Promise<boolean>,
): AutomaticAccessStore & { initialize(): Promise<void> } {
  async function rawAccess<T>(
    mode: IDBTransactionMode,
    operation: (store: IDBObjectStore) => IDBRequest<T>,
  ): Promise<T> {
    const db = await new Promise<IDBDatabase>((resolve, reject) => {
      const request = indexedDB.open("studio-automatic-ui-access-v1", 3);
      request.onupgradeneeded = (event) => {
        if (event.oldVersion === 0)
          request.result.createObjectStore("attempts");
        else {
          const cursor = request
            .transaction!.objectStore("attempts")
            .openCursor();
          cursor.onsuccess = () => {
            const entry = cursor.result;
            if (!entry) return;
            // Strip secrets before any asynchronous credential lookup.
            entry.update(
              attemptMetadata({
                ...entry.value,
                pairStarted: false,
                needsPairCheck: true,
              }),
            );
            entry.continue();
          };
        }
      };
      request.onsuccess = () => resolve(request.result);
      request.onerror = () => reject(request.error);
    });
    return new Promise((resolve, reject) => {
      const tx = db.transaction(
        "attempts",
        mode,
        mode === "readwrite" ? { durability: "strict" } : undefined,
      );
      const request = operation(tx.objectStore("attempts"));
      tx.oncomplete = () => {
        db.close();
        resolve(request.result);
      };
      tx.onabort = tx.onerror = () => {
        db.close();
        reject(
          tx.error ||
            request.error ||
            new Error("The automatic access request could not be saved."),
        );
      };
    });
  }
  let initializing: Promise<void> | null = null;
  const initialize = async (): Promise<void> => {
    if (!initializing) {
      const migrate = async () => {
        const keys = await rawAccess("readonly", (store) => store.getAllKeys());
        for (const key of keys) {
          const attempt = await rawAccess<AutomaticAccessAttempt | undefined>(
            "readonly",
            (store) => store.get(key),
          );
          if (!attempt?.needsPairCheck) continue;
          const started = await hasPairAttempt(attempt);
          await rawAccess("readwrite", (store) =>
            store.put(
              attemptMetadata({
                ...attempt,
                pairStarted: started,
                needsPairCheck: undefined,
              }),
              key,
            ),
          );
        }
      };
      initializing = (async () => {
        if (navigator.locks)
          await navigator.locks.request(
            "studio-automatic-ui-migration-v3",
            migrate,
          );
        else await migrate();
      })().catch((error) => {
        initializing = null;
        throw error;
      });
    }
    await initializing;
  };
  async function access<T>(
    mode: IDBTransactionMode,
    operation: (store: IDBObjectStore) => IDBRequest<T>,
  ) {
    await initialize();
    return rawAccess(mode, operation);
  }
  return {
    initialize,
    read: async (key) =>
      (await access<AutomaticAccessAttempt | undefined>("readonly", (store) =>
        store.get(key),
      )) ?? null,
    save: async (key, value) => {
      await access("readwrite", (store) =>
        store.put(attemptMetadata(value), key),
      );
    },
    remove: async (key) => {
      await access("readwrite", (store) => store.delete(key));
    },
  };
}
const removalKey = "studio-automatic-ui-removals-v1";
export function excludedServers(
  storage = globalThis.localStorage,
): Set<string> {
  const value: unknown = JSON.parse(storage.getItem(removalKey) || "[]");
  if (!Array.isArray(value) || value.some((id) => typeof id !== "string"))
    throw new Error("The saved UI removal list is invalid.");
  return new Set(value);
}
export function setServerExcluded(
  serverId: string,
  excluded: boolean,
  storage = globalThis.localStorage,
) {
  const ids = excludedServers(storage);
  if (excluded) ids.add(serverId);
  else ids.delete(serverId);
  storage.setItem(removalKey, JSON.stringify([...ids]));
}
