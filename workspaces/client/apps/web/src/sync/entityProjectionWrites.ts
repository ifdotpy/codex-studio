const queues = new Map<string, Promise<unknown>>();

/** This page is the only writer of its projection store, so a queue in this
 * page orders the writes. Finish old writes before a reset deletes rows. */
export async function withEntityProjectionWrite<T>(
  databaseName: string,
  write: () => Promise<T>,
): Promise<T> {
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
