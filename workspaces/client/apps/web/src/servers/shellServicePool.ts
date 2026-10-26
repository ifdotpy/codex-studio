import type { StudioServer } from "./registry";

export interface ShellServiceInstance {
  owner: Readonly<StudioServer>;
  ready(): Promise<void>;
  close(): void | Promise<void>;
}
const identity = (server: Readonly<StudioServer>) =>
  JSON.stringify([
    server.id,
    server.origin,
    server.credentialId ?? null,
    server.workspaceId ?? null,
  ]);

/** One root owns every admitted service and every asynchronous open it starts. */
export function createShellServicePool<S extends ShellServiceInstance>(
  open: (server: StudioServer) => S | Promise<S>,
  changed: () => void,
  failed: (error: unknown) => void,
) {
  const slots = new Map<string, { identity: string; service?: S }>();
  const pending = new Set<Promise<void>>();
  const closed = new WeakSet<S>();
  const closeFailures: unknown[] = [];
  let stopped = false;
  let closing: Promise<void> | undefined;
  const track = (work: Promise<void>, ownsClose = false) => {
    pending.add(work);
    void work.then(
      () => pending.delete(work),
      (error) => {
        pending.delete(work);
        if (ownsClose) closeFailures.push(error);
        failed(error);
      },
    );
  };
  const release = (service: S) => {
    if (closed.has(service)) return;
    closed.add(service);
    try {
      track(Promise.resolve(service.close()), true);
    } catch (error) {
      closeFailures.push(error);
      failed(error);
    }
  };
  const settled = async () => {
    while (pending.size) await Promise.allSettled(pending);
  };
  return {
    reconcile(servers: readonly StudioServer[]) {
      if (stopped) throw new Error("The server shell is closed.");
      const wanted = new Map(servers.map((server) => [server.id, server]));
      for (const [id, slot] of slots) {
        if (wanted.has(id) && identity(wanted.get(id)!) === slot.identity)
          continue;
        slots.delete(id);
        if (slot.service) release(slot.service);
      }
      for (const server of servers) {
        if (slots.has(server.id)) continue;
        const slot: { identity: string; service?: S } = {
          identity: identity(server),
        };
        slots.set(server.id, slot);
        const current = () => !stopped && slots.get(server.id) === slot;
        const admit = (service: S) => {
          if (identity(service.owner) !== slot.identity)
            throw new Error("The opened server service has another owner.");
          if (!current()) {
            release(service);
            return false;
          }
          slot.service = service;
          return true;
        };
        try {
          const opened = open({ ...server });
          if (!(opened instanceof Promise)) {
            admit(opened);
            continue;
          }
          track(
            opened.then(async (service) => {
              if (identity(service.owner) !== slot.identity)
                throw new Error("The opened server service has another owner.");
              if (!current()) {
                release(service);
                return;
              }
              try {
                await service.ready();
              } catch (error) {
                release(service);
                throw error;
              }
              if (admit(service)) changed();
            }),
          );
        } catch (error) {
          failed(error);
        }
      }
    },
    snapshot() {
      return new Map(
        [...slots].flatMap(([id, slot]) =>
          slot.service ? [[id, slot.service] as const] : [],
        ),
      );
    },
    settled,
    close() {
      if (closing) return closing;
      stopped = true;
      for (const slot of slots.values())
        if (slot.service) release(slot.service);
      slots.clear();
      closing = settled().then(() => {
        if (closeFailures.length)
          throw new AggregateError(
            closeFailures,
            "The server services did not close.",
          );
      });
      return closing;
    },
  };
}
