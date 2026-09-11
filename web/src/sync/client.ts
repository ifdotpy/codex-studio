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
  const cached = saved<string>("codex-sync-workspace", "");
  const hasCache = /^[a-f0-9]{32}$/.test(cached);
  type Identity = { workspaceId: string; chatState?: boolean };
  const identify = async () => {
    if (navigator.onLine === false)
      throw new TypeError("The device is offline.");
    const identity = await api<Identity>("/api/sync/identity");
    if (!/^[a-f0-9]{32}$/.test(identity.workspaceId))
      throw new Error("Invalid workspace identity");
    return identity;
  };
  const initialIdentity = identify();
  let grace: ReturnType<typeof setTimeout> | undefined;
  let identity: Identity | null = null;
  try {
    identity = await (hasCache
      ? Promise.race([
          initialIdentity,
          new Promise<null>((resolve) => {
            grace = setTimeout(() => resolve(null), 500);
          }),
        ])
      : initialIdentity);
  } catch (error) {
    if (error instanceof ApiError && error.status === 404)
      throw new UnsupportedSyncError();
    const unavailable =
      error instanceof TypeError ||
      (error instanceof ApiError &&
        (error.status >= 500 || [408, 429].includes(error.status)));
    if (!hasCache || !unavailable) throw error;
  } finally {
    clearTimeout(grace);
  }
  const workspaceId = identity?.workspaceId || cached;
  const chatState = identity
    ? identity.chatState === true
    : saved<boolean>(`codex-sync-chat-state:${workspaceId}`, false) === true;
  if (identity) {
    save("codex-sync-workspace", workspaceId);
    save(`codex-sync-chat-state:${workspaceId}`, chatState);
  }
  // Cached display does not authorize reads or draft writes against another Mac.
  // Retain the original request after the grace period, and retry failed checks.
  let verified = !!identity;
  let firstCheck: Promise<Identity> | undefined = initialIdentity;
  let checking: Promise<void> | undefined;
  const verifyWorkspace = () => {
    if (verified) return Promise.resolve();
    if (!checking) {
      const request = firstCheck || identify();
      firstCheck = undefined;
      checking = request
        .then((current) => {
          if (current.workspaceId !== workspaceId)
            throw new Error(
              "The server workspace changed. Reload to synchronize.",
            );
          verified = true;
        })
        .finally(() => {
          checking = undefined;
        });
    }
    return checking;
  };
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
  return { db, workspaceId, chatState, verifyWorkspace };
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
  verifyWorkspace: () => Promise<void>,
) {
  await verifyWorkspace();
  const result = await api(
    `/api/sync/pull?scope=${encodeURIComponent(scope)}&after=${after}&limit=${limit}`,
  );
  if (result.workspaceId !== workspaceId)
    throw new Error("The server workspace changed. Reload to synchronize.");
  return result;
}
// All projection scopes and drafts share one stream and one fallback clock.
// A background PWA must not retain one HTTP connection per conversation.
const invalidations = new Set<() => void>();
let stopInvalidations: (() => void) | undefined;
function watchInvalidations(resync: () => void) {
  invalidations.add(resync);
  if (!stopInvalidations) {
    let source: EventSource | undefined;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const available = () => !document.hidden && navigator.onLine !== false;
    const notify = () => {
      if (!available() || timer !== undefined) return;
      timer = setTimeout(() => {
        timer = undefined;
        if (available()) for (const callback of invalidations) callback();
      }, 50);
    };
    const close = () => {
      source?.close();
      source = undefined;
      clearTimeout(timer);
      timer = undefined;
    };
    const connect = () => {
      if (!available() || source) return;
      source = new EventSource("/api/sync/stream");
      source.onopen = notify;
      source.onmessage = notify;
    };
    const suspend = () => {
      if (!available()) close();
    };
    const stopResume = onResume(() => {
      // A socket can remain OPEN after mobile suspension but never deliver again.
      close();
      connect();
      notify();
    });
    window.addEventListener("offline", suspend);
    document.addEventListener("visibilitychange", suspend);
    // File-backed state still needs periodic reconciliation without SSE events.
    const fallback = setInterval(notify, 3000);
    connect();
    stopInvalidations = () => {
      close();
      clearInterval(fallback);
      stopResume();
      window.removeEventListener("offline", suspend);
      document.removeEventListener("visibilitychange", suspend);
    };
  }
  return () => {
    invalidations.delete(resync);
    if (!invalidations.size) {
      stopInvalidations?.();
      stopInvalidations = undefined;
    }
  };
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
  const { db, workspaceId, chatState, verifyWorkspace } = await syncDatabase();
  const remoteScope = scope === "state" && chatState ? "state:chat" : scope;
  let state = scopes.get(scope);
  if (!state) {
    const events = new Subject<any>();
    const listeners = new Set<(error: unknown | null) => void>();
    const report = (error: unknown | null) =>
      listeners.forEach((listener) => listener(error));
    const replication = replicateRxCollection<SyncDocument, { seq: number }>({
      collection: db.projections,
      // The compact projection has its own checkpoint but keeps the existing
      // local document, so a capability upgrade can show the cached chat first.
      replicationIdentifier: `${workspaceId}:${remoteScope}:v1`,
      live: true,
      retryTime: 3000,
      pull: {
        batchSize: 100,
        handler: async (checkpoint, batchSize) => {
          try {
            const result = await pull(
              remoteScope,
              checkpoint?.seq || 0,
              batchSize,
              workspaceId,
              verifyWorkspace,
            );
            report(null);
            return remoteScope === scope
              ? result
              : {
                  ...result,
                  documents: result.documents.map((document: SyncDocument) =>
                    document.id === remoteScope
                      ? { ...document, id: scope }
                      : document,
                  ),
                };
          } catch (error) {
            report(error);
            throw error;
          }
        },
        stream$: events.asObservable(),
      },
    });
    const stopInvalidation = watchInvalidations(() => events.next("RESYNC"));
    const errors = replication.error$.subscribe((error) => {
      if (error.code !== "RC_PULL") report(error);
    });
    state = {
      users: 0,
      listeners,
      stop: () => {
        stopInvalidation();
        events.complete();
        errors.unsubscribe();
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
  const { db, workspaceId, verifyWorkspace } = await syncDatabase();
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
          pull(
            "drafts",
            checkpoint?.seq || 0,
            batchSize,
            workspaceId,
            verifyWorkspace,
          ),
        ),
      batchSize: 100,
    },
    push: {
      handler: (rows) =>
        attempt("push", async () => {
          await verifyWorkspace();
          return api("/api/sync/drafts", { rows });
        }),
      batchSize: 100,
    },
  });
  const stopInvalidation = watchInvalidations(() => replication.reSync());
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
    stopInvalidation();
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
