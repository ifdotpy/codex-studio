import { createRxDatabase, addRxPlugin } from "rxdb";
import { getRxStorageDexie } from "rxdb/plugins/storage-dexie";
import { RxDBLeaderElectionPlugin } from "rxdb/plugins/leader-election";
import { replicateRxCollection } from "rxdb/plugins/replication";
import type { RxCollection, RxDocumentData } from "rxdb";
import { draftConflictHandler } from "./conflicts";
import { syncApi as api, ApiError, saved, save, setWorkspace } from "../api";

import { onResume } from "./resume";
import {
  cacheTranscript,
  peekTranscript,
  subscribeTranscript,
} from "./transcriptCache";

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
    projections: {
      schema,
      conflictHandler: {
        isEqual: (a: SyncDocument, b: SyncDocument, context: string) =>
          // RxDB checks the pulled master against the current local row here.
          // A lower server sequence is already superseded, including tombstones.
          (context === "downstream-check-if-equal-1" && a.seq < b.seq) ||
          (a.seq === b.seq &&
            a.payload === b.payload &&
            a._deleted === b._deleted),
        resolve: async ({
          realMasterState,
          newDocumentState,
        }: {
          realMasterState: SyncDocument & { _deleted: boolean };
          newDocumentState: SyncDocument & { _deleted: boolean };
        }) =>
          newDocumentState.seq > realMasterState.seq
            ? newDocumentState
            : realMasterState,
      },
    },
    drafts: { schema, conflictHandler: draftConflictHandler },
    outbox: { schema },
  });
  db.projections.$.subscribe((event) => {
    const doc = event.documentData;
    if (doc.id.startsWith("transcript:"))
      cacheTranscript(workspaceId, doc.id.slice(11), doc);
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
export function watchSyncInvalidations(resync: () => void) {
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
// Pull-only projections never run RxDB's upstream scan of every cached chat.
// The storage revision is a compare-and-swap across tabs; retry conflicts against
// the latest row before applying server sequence order, including tombstones.
export async function persistProjection(
  collection: RxCollection<SyncDocument>,
  incoming: SyncDocument,
) {
  for (;;) {
    const [previous] = await collection.storageInstance.findDocumentsById(
      [incoming.id],
      true,
    );
    if (previous && previous.seq >= incoming.seq) return;
    const document: RxDocumentData<SyncDocument> = {
      ...incoming,
      _deleted: incoming._deleted === true,
      _attachments: {},
      _meta: { lwt: 1 },
      _rev: "",
    };
    const result = await collection.storageInstance.bulkWrite(
      [{ previous, document }],
      "studio-projection-pull",
    );
    const failure = result.error[0];
    if (!failure) return;
    if (failure.status !== 409) throw failure;
  }
}
type ProjectionState = {
  users: number;
  foreground: number;
  stop: () => Promise<unknown>;
  refresh: () => Promise<void>;
  listeners: Set<(error: unknown | null) => void>;
};
const scopes = new Map<string, ProjectionState>();
const closingScopes = new Map<string, Promise<unknown>>();
async function acquireProjection(
  scope: string,
  expectedWorkspace?: string,
  background = false,
) {
  const { db, workspaceId, chatState, verifyWorkspace } = await syncDatabase();
  if (expectedWorkspace && expectedWorkspace !== workspaceId)
    throw new Error("The server workspace changed. Reload to synchronize.");
  while (closingScopes.has(scope)) await closingScopes.get(scope);
  const remoteScope = scope === "state" && chatState ? "state:chat" : scope;
  let state = scopes.get(scope);
  if (!state) {
    const listeners = new Set<(error: unknown | null) => void>();
    const report = (error: unknown | null) =>
      listeners.forEach((listener) => listener(error));
    let stopped = false;
    let pending: Promise<void> | undefined;
    let invalidated = false;
    let mappedCheckpoint: number | undefined =
      remoteScope !== scope ? 0 : undefined;
    const refresh = (): Promise<void> => {
      if (stopped) return Promise.resolve();
      if (pending) return pending;
      pending = (async () => {
        do {
          invalidated = false;
          if (
            scopes.get(scope)?.foreground === 0 &&
            (document.hidden || navigator.onLine === false)
          )
            throw new TypeError("The device is offline or the page is hidden.");
          const [previous] =
            await db.projections.storageInstance.findDocumentsById(
              [scope],
              true,
            );
          const result = await pull(
            remoteScope,
            mappedCheckpoint ?? previous?.seq ?? 0,
            100,
            workspaceId,
            verifyWorkspace,
          );
          if (stopped) return;
          for (const document of result.documents as SyncDocument[]) {
            if (document.id !== remoteScope)
              throw new Error(
                "The server returned a different projection scope.",
              );
            await persistProjection(db.projections, { ...document, id: scope });
          }
          if (mappedCheckpoint !== undefined)
            mappedCheckpoint = result.checkpoint.seq;
          report(null);
        } while (invalidated && !stopped);
      })()
        .catch((error) => {
          report(error);
          throw error;
        })
        .finally(() => {
          pending = undefined;
        });
      return pending;
    };
    const stopInvalidation = watchSyncInvalidations(() => {
      invalidated = true;
      void refresh().catch(() => {});
    });
    state = {
      users: 0,
      foreground: 0,
      listeners,
      refresh,
      stop: async () => {
        stopped = true;
        stopInvalidation();
        await pending?.catch(() => {});
      },
    };
    scopes.set(scope, state);
  }
  state.users++;
  if (!background) {
    state.foreground++;
    void state.refresh().catch(() => {});
  }
  const retained = state;
  const release = () => {
    if (!background) retained.foreground--;
    if (--retained.users !== 0) return Promise.resolve();
    scopes.delete(scope);
    const closing = retained.stop().finally(() => {
      if (closingScopes.get(scope) === closing) closingScopes.delete(scope);
    });
    closingScopes.set(scope, closing);
    return closing;
  };
  try {
    if (scope.startsWith("transcript:")) {
      const [doc] = await db.projections.storageInstance.findDocumentsById(
        [scope],
        true,
      );
      if (doc) cacheTranscript(workspaceId, scope.slice(11), doc);
    }
    return { db, workspaceId, state, release };
  } catch (error) {
    await release();
    throw error;
  }
}

export async function watchProjection(
  scope: string,
  accept: (payload: any | null) => void,
  fail: (e: unknown) => void,
) {
  const { db, workspaceId, state, release } = await acquireProjection(scope);
  state.listeners.add(fail);
  const id = scope.startsWith("transcript:") ? scope.slice(11) : null;
  const stopCache = id
    ? subscribeTranscript(workspaceId, id, (entry) => accept(entry.payload))
    : () => {};
  const subscription = db.projections.findOne(scope).$.subscribe((doc: any) => {
    if (!id) {
      accept(doc ? JSON.parse(doc.payload) : null);
      return;
    }
    if (doc) cacheTranscript(workspaceId, id, doc);
    else if (!peekTranscript(workspaceId, id)) accept(null);
  });
  return () => {
    subscription.unsubscribe();
    stopCache();
    state.listeners.delete(fail);
    void release();
  };
}

let prefetches = 0;
const pendingPrefetches = new Map<string, Promise<boolean>>();
/** Refresh one persisted chat without keeping a background subscription alive. */
export function prefetchTranscript(
  workspaceId: string,
  id: string,
): Promise<boolean> {
  if (document.hidden || navigator.onLine === false)
    return Promise.resolve(false);
  const key = `${workspaceId}:${id}`;
  const pending = pendingPrefetches.get(key);
  if (pending) return pending;
  if (prefetches >= 2) return Promise.resolve(false);
  prefetches++;
  const task = (async () => {
    const handle = await acquireProjection(
      `transcript:${id}`,
      workspaceId,
      true,
    );
    let stopCache = () => {};
    try {
      if (document.hidden || navigator.onLine === false) return false;
      // Query the same persisted document used by the foreground view.
      const subscription = handle.db.projections
        .findOne(`transcript:${id}`)
        .$.subscribe((doc: any) => {
          if (doc) cacheTranscript(workspaceId, id, doc);
        });
      stopCache = () => subscription.unsubscribe();
      await handle.state.refresh();
      return true;
    } finally {
      stopCache();
      await handle.release();
    }
  })().finally(() => {
    prefetches--;
    pendingPrefetches.delete(key);
  });
  pendingPrefetches.set(key, task);
  return task;
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
          return api("/api/sync/drafts", { rows }, { workspaceId });
        }),
      batchSize: 100,
    },
  });
  const stopInvalidation = watchSyncInvalidations(() => replication.reSync());
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
