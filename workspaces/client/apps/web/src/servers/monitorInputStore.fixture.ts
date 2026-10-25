import { validMonitorInput, type MonitorInputStore } from "./monitorInputStore";

// Deterministic transaction fixture. Real IndexedDB serialization has a browser witness.
export function monitorInputStoreFixture() {
  const rows = new Map<string, unknown>();
  let queue = Promise.resolve();
  let failure: Error | undefined;
  let pause: { entered(): void; wait: Promise<void> } | undefined;
  const key = (owner: string | null, id: string) =>
    JSON.stringify([owner || "local", id]);
  return {
    read: (owner: string | null, id: string) =>
      structuredClone(rows.get(key(owner, id))),
    corrupt: (owner: string | null, id: string, raw: unknown) =>
      rows.set(key(owner, id), raw),
    failNext: (error: Error) => {
      failure = error;
    },
    pauseNextCommit() {
      let entered!: () => void;
      let release!: () => void;
      const started = new Promise<void>((resolve) => {
        entered = resolve;
      });
      const wait = new Promise<void>((resolve) => {
        release = resolve;
      });
      pause = { entered, wait };
      return { started, release };
    },
    store(owner: string | null): MonitorInputStore {
      return {
        transition(id, update) {
          const operation = queue.then(async () => {
            const current = validMonitorInput(
              structuredClone(rows.get(key(owner, id))),
            );
            const next = update(current);
            const paused = pause;
            pause = undefined;
            if (paused) {
              paused.entered();
              await paused.wait;
            }
            if (failure) {
              const error = failure;
              failure = undefined;
              throw error;
            }
            if (next.entry === undefined) rows.delete(key(owner, id));
            else
              rows.set(
                key(owner, id),
                structuredClone(validMonitorInput(next.entry)),
              );
            return next.result;
          });
          queue = operation.then(
            () => {},
            () => {},
          );
          return operation;
        },
      };
    },
  };
}
