import { LocalCollection, type Row } from "./localCollection";

/**
 * Drafts follow the server. The page keeps a draft only while the server has
 * not confirmed it (unsent input). This module pulls the server's drafts and
 * pushes the unsent ones, with the same wire protocol as before:
 * `assumedMasterState` names the server row an edit was based on, and the
 * server answers with the rows that conflict.
 */

export type ConflictHandler = {
  isEqual(a: Row, b: Row, context: string): boolean;
  resolve(input: { realMasterState: Row; newDocumentState: Row }): Promise<Row>;
};
export type PushRow = { newDocumentState: Row; assumedMasterState?: Row };
export type PullResult = { documents: Row[]; checkpoint: { seq: number } };

export class Subject<T> {
  private readonly listeners = new Set<(value: T) => void>();
  private current: T | undefined;
  constructor(initial?: T) {
    this.current = initial;
  }
  getValue() {
    return this.current as T;
  }
  next(value: T) {
    this.current = value;
    for (const listener of this.listeners) listener(value);
  }
  subscribe(listener: (value: T) => void) {
    this.listeners.add(listener);
    return { unsubscribe: () => void this.listeners.delete(listener) };
  }
}

const sameState = (a: Row, b: Row) =>
  a.payload === b.payload && (a._deleted === true) === (b._deleted === true);

/** The drafts collection with its server state and its unsent rows. */
export function createDraftStore(name: string, handler: ConflictHandler) {
  const masters = new Map<string, Row>();
  const pending = new Map<string, Row>();
  const collection = new LocalCollection(name, {
    durable: {
      async put(row) {
        pending.set(row.id, row);
      },
    },
  });

  async function writeMaster(row: Row) {
    for (;;) {
      const [previous] = await collection.storageInstance.findDocumentsById(
        [row.id],
        true,
      );
      if (previous && sameState(previous, row)) return;
      const result = await collection.storageInstance.bulkWrite(
        [{ ...(previous ? { previous } : {}), document: row }],
        "master",
      );
      if (!result.error.length) return;
    }
  }

  async function settle(id: string) {
    pending.delete(id);
  }

  /** Take a row from the server, keeping an unsent local edit on top of it. */
  async function applyMaster(master: Row) {
    masters.set(master.id, master);
    const local = pending.get(master.id);
    if (!local) return writeMaster(master);
    if (sameState(master, local)) {
      await settle(master.id);
      return writeMaster(master);
    }
    // An echo of an older push of this page is not a conflict.
    if (handler.isEqual(master, local, "downstream-check-if-equal-1")) return;
    const resolved = await handler.resolve({
      realMasterState: master,
      newDocumentState: local,
    });
    if (sameState(resolved, master)) {
      await settle(master.id);
      return writeMaster(master);
    }
    pending.set(master.id, resolved);
    await writeMaster(resolved);
  }

  return {
    collection,
    applyMaster,
    pendingRows(): PushRow[] {
      return [...pending.values()].map((row) => {
        const assumed = masters.get(row.id);
        return {
          newDocumentState: row,
          ...(assumed ? { assumedMasterState: assumed } : {}),
        };
      });
    },
    /** The server took these rows, except the ones it returned as conflicts. */
    async pushed(rows: PushRow[], conflicts: readonly Row[]) {
      const conflicted = new Map(conflicts.map((row) => [row.id, row]));
      for (const { newDocumentState: sent } of rows) {
        if (conflicted.has(sent.id)) continue;
        masters.set(sent.id, sent);
        const latest = pending.get(sent.id);
        if (latest && sameState(latest, sent)) await settle(sent.id);
      }
      for (const master of conflicted.values()) await applyMaster(master);
    },
  };
}
export type DraftStore = ReturnType<typeof createDraftStore>;

export type ReplicationOptions = {
  retryTime: number;
  pull: {
    handler: (
      checkpoint: { seq: number } | undefined,
      batchSize: number,
    ) => Promise<PullResult>;
    batchSize: number;
  };
  push: {
    handler: (rows: PushRow[]) => Promise<Row[]>;
    waitBeforePersist?: () => Promise<void>;
    batchSize: number;
  };
};

export type ReplicationError = {
  code: "RC_PULL" | "RC_PUSH";
  error: unknown;
};

/** Pull the server's drafts and push the unsent ones, until cancelled. */
export function createDraftReplication(
  store: DraftStore,
  options: ReplicationOptions,
) {
  const error$ = new Subject<ReplicationError>();
  const sent$ = new Subject<PushRow[]>();
  const activeDown = new Subject<boolean>(false);
  const firstSyncDown = new Subject<boolean>(false);
  let cancelled = false;
  let checkpoint: { seq: number } | undefined;
  let pulling: Promise<void> | undefined;
  let pullAgain = false;
  let pushing: Promise<void> | undefined;
  let pushAgain = false;
  let applying: Promise<void> | undefined;
  let pullRetry: ReturnType<typeof setTimeout> | undefined;
  let pushRetry: ReturnType<typeof setTimeout> | undefined;

  const pull = (): Promise<void> => {
    if (pulling) {
      pullAgain = true;
      return pulling;
    }
    clearTimeout(pullRetry);
    pulling = (async () => {
      activeDown.next(true);
      try {
        do {
          pullAgain = false;
          for (;;) {
            if (cancelled) return;
            const result = await options.pull.handler(
              checkpoint,
              options.pull.batchSize,
            );
            for (const document of result.documents)
              await store.applyMaster(document);
            checkpoint = result.checkpoint;
            if (result.documents.length < options.pull.batchSize) break;
          }
        } while (pullAgain && !cancelled);
        firstSyncDown.next(true);
      } catch (error) {
        error$.next({ code: "RC_PULL", error });
        if (!cancelled)
          pullRetry = setTimeout(() => void pull(), options.retryTime);
      } finally {
        pulling = undefined;
        activeDown.next(false);
      }
    })();
    return pulling;
  };

  const push = (): Promise<void> => {
    if (pushing) {
      pushAgain = true;
      void options.push.waitBeforePersist?.();
      return pushing;
    }
    clearTimeout(pushRetry);
    pushing = (async () => {
      try {
        do {
          pushAgain = false;
          for (;;) {
            // Edits made while a push runs, or while we wait, go in one batch.
            await options.push.waitBeforePersist?.();
            if (cancelled) return;
            const rows = store.pendingRows().slice(0, options.push.batchSize);
            if (!rows.length) break;
            const conflicts = await options.push.handler(rows);
            await store.pushed(rows, conflicts);
            sent$.next(rows);
          }
        } while (pushAgain && !cancelled);
      } catch (error) {
        error$.next({ code: "RC_PUSH", error });
        if (!cancelled)
          pushRetry = setTimeout(() => void push(), options.retryTime);
      } finally {
        pushing = undefined;
      }
    })();
    return pushing;
  };

  const localWrites = store.collection.$.subscribe((change) => {
    if (change.context === "local" && !cancelled) void push();
  });
  void pull();
  if (store.pendingRows().length) void push();

  return {
    error$,
    sent$,
    startPromise: Promise.resolve(),
    internalReplicationState: {
      events: { active: { down: activeDown } },
      firstSyncDone: { down: firstSyncDown },
    },
    /** Pull again; the pull handler decides whether it has work. */
    reSync() {
      void pull();
    },
    /** Take rows that the stream already delivered, without a pull. */
    emitEvent(batch: PullResult) {
      const run = async () => {
        for (const document of batch.documents)
          await store.applyMaster(document);
        checkpoint = batch.checkpoint;
      };
      const task = (applying ?? Promise.resolve())
        .then(run)
        .catch((error) => error$.next({ code: "RC_PULL", error }));
      applying = task;
      void task.then(() => {
        if (applying === task) applying = undefined;
      });
    },
    /** Wait until no pull or push runs and no unsent draft is left. */
    async awaitInSync() {
      for (;;) {
        const busy = pulling ?? pushing ?? applying;
        if (busy) await busy;
        else if (store.pendingRows().length && !cancelled)
          await new Promise((resolve) => setTimeout(resolve, 10));
        else return;
      }
    },
    async awaitInitialReplication() {
      if (firstSyncDown.getValue()) return;
      await new Promise<void>((resolve) => {
        const subscription = firstSyncDown.subscribe((done) => {
          if (!done) return;
          subscription.unsubscribe();
          resolve();
        });
      });
    },
    async cancel() {
      cancelled = true;
      localWrites.unsubscribe();
      clearTimeout(pullRetry);
      clearTimeout(pushRetry);
      await Promise.allSettled([pulling, pushing, applying]);
    },
  };
}
export type DraftReplication = ReturnType<typeof createDraftReplication>;
