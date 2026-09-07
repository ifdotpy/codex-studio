import { createRxDatabase, addRxPlugin } from "rxdb";
import { getRxStorageDexie } from "rxdb/plugins/storage-dexie";
import { RxDBLeaderElectionPlugin } from "rxdb/plugins/leader-election";
import { replicateRxCollection } from "rxdb/plugins/replication";
import { Subject } from "rxjs";
import { draftConflictHandler } from "./conflicts";
import { syncApi as api, ApiError, saved, save, setWorkspace } from "../api";

import { onResume } from "./resume";

addRxPlugin(RxDBLeaderElectionPlugin);
export class UnsupportedSyncError extends Error {
  constructor() {
    super("This server does not support synchronization.");
  }
}
export type SyncDocument = {
  id: string;
  payload: string;
  seq: number;
  _deleted?: boolean;
};
const schema = {
  version: 0,
  primaryKey: "id",
  type: "object",
  properties: {
    id: { type: "string", maxLength: 300 },
    payload: { type: "string" },
    seq: {
      type: "integer",
      minimum: 0,
      maximum: 9007199254740991,
      multipleOf: 1,
    },
  },
  required: ["id", "payload", "seq"],
} as const;
let pending: ReturnType<typeof open> | undefined;
async function open() {
  let workspaceId: string;
  try {
    ({ workspaceId } = await api<{ workspaceId: string }>(
      "/api/sync/identity",
    ));
    if (!/^[a-f0-9]{32}$/.test(workspaceId))
      throw new Error("Invalid workspace identity");
    save("codex-sync-workspace", workspaceId);
  } catch (error) {
    if (error instanceof ApiError && error.status === 404)
      throw new UnsupportedSyncError();
    const unavailable =
      error instanceof TypeError ||
      (error instanceof ApiError &&
        (error.status >= 500 || [408, 429].includes(error.status)));
    if (!unavailable) throw error;
    workspaceId = saved("codex-sync-workspace", "");
    if (!workspaceId) throw error;
  }
  setWorkspace(workspaceId);
  const db = await createRxDatabase({
    name: `studio${workspaceId}`,
    storage: getRxStorageDexie(),
    multiInstance: true,
  });
  await db.addCollections({
    projections: { schema },
    drafts: { schema, conflictHandler: draftConflictHandler },
    outbox: { schema },
  });
  return { db, workspaceId };
}
export function syncDatabase() {
  return (pending ??= open().catch((error) => {
    pending = undefined;
    throw error;
  }));
}

async function pull(
  scope: string,
  after: number,
  limit: number,
  workspaceId: string,
) {
  const result = await api(
    `/api/sync/pull?scope=${encodeURIComponent(scope)}&after=${after}&limit=${limit}`,
  );
  if (result.workspaceId !== workspaceId)
    throw new Error("The server workspace changed. Reload to synchronize.");
  return result;
}
const scopes = new Map<
  string,
  {
    users: number;
    stop: () => void;
    listeners: Set<(error: unknown | null) => void>;
  }
>();
export async function watchProjection(
  scope: string,
  accept: (payload: any | null) => void,
  fail: (e: unknown) => void,
) {
  const { db, workspaceId } = await syncDatabase();
  let state = scopes.get(scope);
  if (!state) {
    const events = new Subject<any>();
    const listeners = new Set<(error: unknown | null) => void>();
    const report = (error: unknown | null) =>
      listeners.forEach((listener) => listener(error));
    const replication = replicateRxCollection<SyncDocument, { seq: number }>({
      collection: db.projections,
      replicationIdentifier: `${workspaceId}:${scope}:v1`,
      live: true,
      retryTime: 3000,
      pull: {
        batchSize: 100,
        handler: async (checkpoint, batchSize) => {
          try {
            const result = await pull(
              scope,
              checkpoint?.seq || 0,
              batchSize,
              workspaceId,
            );
            report(null);
            return result;
          } catch (error) {
            report(error);
            throw error;
          }
        },
        stream$: events.asObservable(),
      },
    });
    const source = new EventSource("/api/sync/stream");
    const resync = () => events.next("RESYNC");
    source.onopen = resync;
    source.onmessage = resync;
    // Reconcile file-backed state too, and recover missed notifications after suspension.
    const timer = setInterval(resync, 3000);
    const stopResume = onResume(resync);
    const errors = replication.error$.subscribe((error) => {
      if (error.code !== "RC_PULL") report(error);
    });
    state = {
      users: 0,
      listeners,
      stop: () => {
        clearInterval(timer);
        source.close();
        events.complete();
        errors.unsubscribe();
        stopResume();
        void replication.cancel();
      },
    };
    scopes.set(scope, state);
  }
  state.users++;
  state.listeners.add(fail);
  const subscription = db.projections.findOne(scope).$.subscribe((doc: any) => {
    if (doc) accept(JSON.parse(doc.payload));
    else accept(null);
  });
  return () => {
    subscription.unsubscribe();
    state!.listeners.delete(fail);
    if (--state!.users === 0) {
      state!.stop();
      scopes.delete(scope);
    }
  };
}

export async function startDraftReplication(
  report: (e: unknown | null) => void,
) {
  const { db, workspaceId } = await syncDatabase();
  const failures = new Map<string, unknown>();
  let stopped = false;
  const state = (direction: string, error: unknown | null) => {
    if (stopped) return;
    if (error === null) failures.delete(direction);
    else failures.set(direction, error);
    report(failures.size ? failures.values().next().value : null);
  };
  const attempt = async <T>(direction: string, request: () => Promise<T>) => {
    try {
      const result = await request();
      // Empty pulls also prove recovery. One direction cannot clear the other.
      state(direction, null);
      return result;
    } catch (error) {
      state(direction, error);
      throw error;
    }
  };
  const replication = replicateRxCollection<SyncDocument, { seq: number }>({
    collection: db.drafts,
    replicationIdentifier: `${workspaceId}:drafts:v1`,
    live: true,
    retryTime: 3000,
    pull: {
      handler: (checkpoint, batchSize) =>
        attempt("pull", () =>
          pull("drafts", checkpoint?.seq || 0, batchSize, workspaceId),
        ),
      batchSize: 100,
    },
    push: {
      handler: (rows) =>
        attempt("push", () => api("/api/sync/drafts", { rows })),
      batchSize: 100,
    },
  });
  const resync = () => replication.reSync();
  const timer = setInterval(resync, 3000);
  const stopResume = onResume(resync);
  const errors = replication.error$.subscribe((error) => {
    const direction =
      error.code === "RC_PULL"
        ? "pull"
        : error.code === "RC_PUSH"
          ? "push"
          : "replication";
    state(direction, error);
  });
  return () => {
    stopped = true;
    stopResume();
    clearInterval(timer);
    errors.unsubscribe();
    void replication.cancel();
  };
}

// Reopen a subscription after an initial connection failure, without a page reload.
export function subscribeProjection(
  scope: string,
  accept: (payload: any | null) => void,
  report: (error: unknown | null) => void,
) {
  let stopped = false,
    connecting = false,
    attached = false;
  let dispose = () => {};
  let timer: ReturnType<typeof setTimeout> | undefined;
  const connect = async () => {
    if (stopped || connecting || attached) return;
    clearTimeout(timer);
    connecting = true;
    try {
      const stop = await watchProjection(
        scope,
        (value) => {
          if (!stopped) accept(value);
        },
        (error) => {
          if (!stopped) report(error);
        },
      );
      if (stopped) stop();
      else {
        dispose = stop;
        attached = true;
      }
    } catch (error) {
      if (!stopped && !(error instanceof UnsupportedSyncError)) {
        report(error);
        timer = setTimeout(() => void connect(), 3000);
      }
    } finally {
      connecting = false;
    }
  };
  const stopResume = onResume(() => void connect());
  void connect();
  return () => {
    stopped = true;
    clearTimeout(timer);
    stopResume();
    dispose();
  };
}
