export const ENTITY_PROJECTION_DATABASE_PREFIX = "studio-entity-projection-";
export const PROJECTION_DATABASE_RELOAD_MESSAGE =
  "The local workspace cache was closed by another tab. Reload Studio to continue.";

export function isProjectionDatabaseClosedError(error: unknown) {
  const message = error instanceof Error ? error.message : String(error);
  return /database has been closed|RxStorageInstanceDexie is closed|RxCollection is closed or removed/i.test(
    message,
  );
}

export function entityProjectionDatabaseName(
  workspaceId: string,
  schemaHash: string,
  serverSuffix = "",
) {
  return `${ENTITY_PROJECTION_DATABASE_PREFIX}${workspaceId}${serverSuffix}-${schemaHash}`;
}

function projectionDatabaseLockName(databaseName: string) {
  return `studio-entity-projection-database:${databaseName}`;
}

const heldProjectionDatabaseLocks = new Set<string>();

/** Keep this tab's projection database alive until its browsing context unloads. */
export async function holdEntityProjectionDatabaseLock(databaseName: string) {
  if (typeof navigator === "undefined" || !navigator.locks?.request)
    return false;
  const lockName = projectionDatabaseLockName(databaseName);
  if (heldProjectionDatabaseLocks.has(lockName)) return true;

  let resolveAcquired!: () => void;
  let rejectAcquired!: (error: unknown) => void;
  let hold!: () => void;
  const acquired = new Promise<void>((resolve, reject) => {
    resolveAcquired = resolve;
    rejectAcquired = reject;
  });
  const lifetime = new Promise<void>((resolve) => {
    hold = resolve;
  });
  heldProjectionDatabaseLocks.add(lockName);
  const request = navigator.locks.request(
    lockName,
    { mode: "shared" },
    async (lock) => {
      if (!lock)
        throw new Error("Projection database lifetime lock unavailable");
      resolveAcquired();
      await lifetime;
    },
  );
  void request.catch((error: unknown) => {
    heldProjectionDatabaseLocks.delete(lockName);
    rejectAcquired(error);
  });
  try {
    await acquired;
    return true;
  } catch {
    // Without a held lease cleanup must remain disabled for this tab.
    hold();
    return false;
  }
}

function withProjectionDatabaseLock<T>(
  databaseName: string,
  run: (lock: Lock | null) => Promise<T> | T,
) {
  if (typeof navigator === "undefined" || !navigator.locks?.request)
    return Promise.resolve(undefined);
  return navigator.locks.request(
    projectionDatabaseLockName(databaseName),
    { mode: "exclusive", ifAvailable: true },
    run,
  );
}

function deleteProjectionDatabase(name: string) {
  return new Promise<void>((resolve, reject) => {
    try {
      const request = indexedDB.deleteDatabase(name);
      request.onsuccess = () => resolve();
      request.onerror = () => reject(request.error);
      request.onblocked = () => {
        console.error(
          `Projection database deletion unexpectedly blocked: ${name}`,
        );
      };
    } catch (error) {
      reject(error);
    }
  });
}

/** Schedule best-effort cleanup without making it part of database startup. */
export function deleteOtherEntityProjectionDatabases(
  workspaceId: string,
  currentSchemaHash: string,
  serverSuffix = "",
) {
  if (!/^[a-f0-9]{32}$/.test(workspaceId)) return;
  if (!/^[a-f0-9]{64}$/.test(currentSchemaHash)) return;
  if (!/^[a-z0-9]*$/.test(serverSuffix)) return;
  if (typeof indexedDB === "undefined") return;
  if (typeof navigator === "undefined" || !navigator.locks?.request) return;
  try {
    const databaseNames = indexedDB.databases;
    if (typeof databaseNames !== "function") return;
    void Promise.resolve(databaseNames.call(indexedDB))
      .then((databases) => {
        const storagePrefix = `rxdb-dexie-${ENTITY_PROJECTION_DATABASE_PREFIX}`;
        const currentWorkspacePrefix = `${storagePrefix}${workspaceId}${serverSuffix}-`;
        const staleDatabases = new Map<string, string[]>();
        for (const { name } of databases) {
          if (!name?.startsWith(currentWorkspacePrefix)) continue;
          const suffix = name.slice(currentWorkspacePrefix.length);
          const match =
            /^([a-f0-9]{64})--0--(?:projections|_rxdb_internal|rx-replication-meta-[a-f0-9]{64})$/.exec(
              suffix,
            );
          if (!match || match[1] === currentSchemaHash) continue;
          const logicalName = name.slice(
            "rxdb-dexie-".length,
            name.indexOf("--0--"),
          );
          const names = staleDatabases.get(logicalName) ?? [];
          names.push(name);
          staleDatabases.set(logicalName, names);
        }
        for (const [logicalName, names] of staleDatabases) {
          void withProjectionDatabaseLock(logicalName, async (lock) => {
            if (!lock) return;
            for (const name of names)
              await deleteProjectionDatabase(name).catch(() => {});
          }).catch(() => {});
        }
      })
      .catch(() => {});
  } catch {
    // A synchronous browser storage failure must not affect startup either.
  }
}
