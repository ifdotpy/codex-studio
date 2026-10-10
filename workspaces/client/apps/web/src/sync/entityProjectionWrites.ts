/** The shared IndexedDB projection must finish old writes before a reset deletes rows. */
export async function withEntityProjectionWrite<T>(
  databaseName: string,
  write: () => Promise<T>,
): Promise<T> {
  if (typeof navigator !== "undefined" && navigator.locks?.request)
    return await navigator.locks.request(
      `studio-entity-projection:${databaseName}`,
      write,
    );
  return write();
}

export function canSerializeEntityProjectionWrites() {
  return typeof navigator !== "undefined" && !!navigator.locks?.request;
}
