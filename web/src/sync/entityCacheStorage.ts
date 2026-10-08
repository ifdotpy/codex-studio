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
  try {
    const databaseNames = indexedDB.databases;
    if (typeof databaseNames !== "function") return;
    void Promise.resolve(databaseNames.call(indexedDB))
      .then((databases) => {
        const storagePrefix = `rxdb-dexie-${ENTITY_PROJECTION_DATABASE_PREFIX}`;
        const currentWorkspacePrefix = `${storagePrefix}${workspaceId}${serverSuffix}-`;
        for (const { name } of databases) {
          if (!name?.startsWith(currentWorkspacePrefix)) continue;
          const suffix = name.slice(currentWorkspacePrefix.length);
          const match =
            /^([a-f0-9]{64})--0--(?:projections|_rxdb_internal|rx-replication-meta-[a-f0-9]{64})$/.exec(
              suffix,
            );
          if (!match || match[1] === currentSchemaHash) continue;
          try {
            const request = indexedDB.deleteDatabase(name);
            // Some browsers may leave this request blocked while another tab
            // owns the database. A later schema-confirmed open retries cleanup.
            request.onblocked = () => {};
            request.onerror = () => {};
          } catch {
            // Cache cleanup must not delay or fail workspace startup.
          }
        }
      })
      .catch(() => {});
  } catch {
    // A synchronous browser storage failure must not affect startup either.
  }
}
