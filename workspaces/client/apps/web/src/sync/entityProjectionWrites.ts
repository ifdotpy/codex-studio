const queues = new Map<string, Promise<unknown>>();

/** Name prefix of the native core's cache. It has one writer, this page, so a
 * queue in this page replaces Web Locks. */
export const NATIVE_PROJECTION_PREFIX = "native-projection:";

/** The shared IndexedDB projection must finish old writes before a reset deletes rows. */
export async function withEntityProjectionWrite<T>(
  databaseName: string,
  write: () => Promise<T>,
): Promise<T> {
  if (databaseName.startsWith(NATIVE_PROJECTION_PREFIX)) {
    const run = (queues.get(databaseName) ?? Promise.resolve()).then(
      write,
      write,
    );
    queues.set(
      databaseName,
      run.catch(() => {}),
    );
    return run;
  }
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
