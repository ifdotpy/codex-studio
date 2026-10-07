export const ENTITY_PROJECTION_DATABASE_PREFIX = "studio-entity-projection-";

export function entityProjectionDatabaseName(
  workspaceId: string,
  schemaHash: string,
) {
  return `${ENTITY_PROJECTION_DATABASE_PREFIX}${workspaceId}-${schemaHash}`;
}

/** Schedule best-effort cleanup without making it part of database startup. */
export function deleteOtherEntityProjectionDatabases(
  workspaceId: string,
  currentDatabaseName: string,
) {
  if (typeof indexedDB === "undefined") return;
  const databaseNames = indexedDB.databases;
  if (typeof databaseNames !== "function") return;
  try {
    void Promise.resolve(databaseNames.call(indexedDB))
      .then((databases) => {
        const storagePrefix = `rxdb-dexie-${ENTITY_PROJECTION_DATABASE_PREFIX}`;
        const currentWorkspacePrefix = `${storagePrefix}${workspaceId}-`;
        const currentStoragePrefix = `rxdb-dexie-${currentDatabaseName}--`;
        for (const { name } of databases) {
          if (
            !name ||
            !name.startsWith(currentWorkspacePrefix) ||
            name.startsWith(currentStoragePrefix)
          )
            continue;
          try {
            const request = indexedDB.deleteDatabase(name);
            // A live tab can keep this deletion pending until its connection closes.
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
