import { createRxDatabase, addRxPlugin } from "rxdb";
import { getRxStorageDexie } from "rxdb/plugins/storage-dexie";
import { RxDBLeaderElectionPlugin } from "rxdb/plugins/leader-election";
import { replicateRxCollection } from "rxdb/plugins/replication";
import type { RxCollection, RxDocumentData } from "rxdb";
import { draftConflictHandler } from "./conflicts";
import { applyEntityRows, emptyEntityProjection } from "./entityProjection";
import { syncApi as api, ApiError, saved, save, setWorkspace } from "../api";

import { onResume } from "./resume";
import {
  clearWorkspaceTokenRates,
  receiveWorkspaceTokenRates,
} from "../tokenRate";
import type { TokenRate } from "../tokenRate";
import {
  cacheTranscript,
  cacheTranscriptValue,
  peekTranscript,
  patchTranscriptValue,
  subscribeTranscript,
} from "./transcriptCache";

addRxPlugin(RxDBLeaderElectionPlugin);
export type SyncDocument = {
  id: string;
  payload: string;
  seq: number;
  _deleted?: boolean;
};
class WorkspaceMismatchError extends Error {
  constructor() {
    super("The server workspace changed. Reload to synchronize.");
  }
}
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
  type Identity = { workspaceId: string };
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
    const unavailable =
      error instanceof TypeError ||
      (error instanceof ApiError &&
        (error.status >= 500 || [408, 429].includes(error.status)));
    if (!hasCache || !unavailable) throw error;
  } finally {
    clearTimeout(grace);
  }
  const workspaceId = identity?.workspaceId || cached;
  if (identity) save("codex-sync-workspace", workspaceId);
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
            throw new WorkspaceMismatchError();
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
    if (doc.id.startsWith("transcript:") && doc.id.split(":").length === 2)
      void cacheStoredTranscript(db.projections, workspaceId, doc.id, doc);
  });
  return { db, workspaceId, verifyWorkspace };
}
export function syncDatabase() {
  return (pending ??= open().catch((error) => {
    pending = undefined;
    throw error;
  }));
}

if (typeof window !== "undefined")
  window.addEventListener("codex-sync-entities", (event: Event) => {
    const detail = (event as CustomEvent).detail;
    if (!detail || !Array.isArray(detail.documents)) return;
    void syncDatabase()
      .then(async ({ db, workspaceId }) => {
        if (detail.workspaceId && detail.workspaceId !== workspaceId) return;
        for (const document of detail.documents as SyncDocument[]) {
          if (!document.id.startsWith("entity:")) continue;
          await persistProjection(db.projections, document);
        }
      })
      .catch(() => {});
  });

async function pull(
  scope: string,
  after: number,
  limit: number,
  workspaceId: string,
  verifyWorkspace: () => Promise<void>,
  initialHigh?: number,
  priorityId?: string | null,
) {
  await verifyWorkspace();
  const fresh =
    initialHigh !== undefined ? `&fresh=1&initialHigh=${initialHigh}` : "";
  const reset = scope === "state:entities:v1" ? "&reset=1" : "";
  const priority = priorityId
    ? `&priorityId=${encodeURIComponent(priorityId)}`
    : "";
  const result = await api(
    `/api/sync/pull?scope=${encodeURIComponent(scope)}&after=${after}&limit=${limit}${fresh}${reset}${priority}`,
  );
  if (result.workspaceId !== workspaceId) throw new WorkspaceMismatchError();
  return result;
}
// All projections and drafts share one stream with scope-aware callbacks.
// A background PWA must not retain one HTTP connection per conversation.
type TranscriptRevisions = {
  workspaceId: string;
  revisions: Record<string, number> | null;
};
let latestTranscriptRevisions: TranscriptRevisions | undefined;
const revisionListeners = new Set<(value: TranscriptRevisions) => void>();
function receiveTranscriptRevisions(value: {
  workspaceId?: string;
  transcriptRevisions?: Record<string, number>;
}) {
  const revisions = value.transcriptRevisions;
  if (
    revisions &&
    (typeof revisions !== "object" ||
      Array.isArray(revisions) ||
      Object.entries(revisions).some(
        ([id, revision]) =>
          !id ||
          id.length >= 300 ||
          !Number.isSafeInteger(revision) ||
          revision < 0,
      ))
  )
    return;
  latestTranscriptRevisions = {
    workspaceId: value.workspaceId!,
    revisions: revisions ?? null,
  };
  for (const listener of revisionListeners) listener(latestTranscriptRevisions);
}
/** Use the existing workspace stream to identify chats that need a fresh page. */
export function watchTranscriptRevisions(
  workspaceId: string,
  accept: (revisions: Record<string, number> | null) => void,
) {
  const listener = (value: TranscriptRevisions) => {
    if (value.workspaceId === workspaceId) accept(value.revisions);
  };
  revisionListeners.add(listener);
  if (latestTranscriptRevisions) listener(latestTranscriptRevisions);
  const stop = watchSyncInvalidations("state", () => {});
  return () => {
    revisionListeners.delete(listener);
    stop();
  };
}
const invalidations = new Map<() => void, string>();
let stopInvalidations: (() => void) | undefined;
export function watchSyncInvalidations(
  resync: () => void,
  scope?: string,
): () => void;
export function watchSyncInvalidations(
  scope: string,
  resync: () => void,
): () => void;
export function watchSyncInvalidations(
  first: string | (() => void),
  second: string | (() => void) = "legacy",
) {
  const scope = typeof first === "string" ? first : String(second);
  const resync = (typeof first === "function" ? first : second) as () => void;
  const generationScope =
    scope === "state" ||
    scope === "state:chat" ||
    scope === "entities" ||
    scope === "legacy"
      ? "state"
      : scope.startsWith("transcript:")
        ? "transcripts"
        : scope;
  // One origin-wide stream leaves HTTP connections for reads and commands.
  // All scopes reconcile through their existing workspace-guarded pull.
  invalidations.set(resync, generationScope);
  const ensureCoordinator = () => {
    if (!stopInvalidations) {
      const fallbackPollMs = 3000;
      const heartbeatMs = 1000;
      const coordinatorTimeoutMs = 3500;
      let stopped = false;
      let source: EventSource | undefined;
      let channel: BroadcastChannel | undefined;
      let flushTimer: ReturnType<typeof setTimeout> | undefined;
      let pollTimer: ReturnType<typeof setInterval> | undefined;
      let heartbeatTimer: ReturnType<typeof setInterval> | undefined;
      let watchdogTimer: ReturnType<typeof setInterval> | undefined;
      let lastGenerations: Record<string, number> | undefined;
      let latestTokenRates: {
        rates: Record<string, TokenRate | null>;
        teams: Record<string, Record<string, TokenRate | null>>;
      } = { rates: {}, teams: {} };
      let lastWorkspaceId: string | undefined;
      let pendingScopes = new Set<string>();
      let pendingFullRefresh = false;
      let openedStream = false;
      let workspaceId: string | undefined;
      let isOwner = false;
      let lastHeartbeatAt = 0;
      let ownerLockPending = false;
      let releaseOwnerLock: (() => void) | undefined;
      const available = () => !document.hidden && navigator.onLine !== false;
      const notify = (changed?: Set<string>) => {
        if (!available()) return;
        if (changed)
          for (const changedScope of changed) pendingScopes.add(changedScope);
        else pendingFullRefresh = true;
        if (flushTimer !== undefined) return;
        flushTimer = setTimeout(() => {
          flushTimer = undefined;
          const fullRefresh = pendingFullRefresh;
          const scopes = pendingScopes;
          pendingFullRefresh = false;
          pendingScopes = new Set();
          if (available())
            for (const [callback, subscribed] of invalidations)
              if (fullRefresh || scopes.has(subscribed)) callback();
        }, 50);
      };
      const applyGenerations = (current: Record<string, number>) => {
        const changed = new Set(
          Object.keys(current).filter(
            (changedScope) =>
              lastGenerations?.[changedScope] !== current[changedScope],
          ),
        );
        lastGenerations = current;
        notify(changed);
      };
      const applyGenerationState = (
        value: {
          protocol?: number;
          workspaceId?: string;
          generations?: Record<string, number>;
          transcriptRevisions?: Record<string, number>;
        },
        acceptRevisions = true,
      ) => {
        const generations = value?.generations;
        const expected = ["drafts", "state", "transcripts"];
        const valid =
          value?.protocol === 2 &&
          /^[a-f0-9]{32}$/.test(value.workspaceId || "") &&
          generations &&
          Object.keys(generations).sort().join(",") === expected.join(",") &&
          expected.every(
            (changedScope) =>
              Number.isSafeInteger(generations[changedScope]) &&
              generations[changedScope] >= 0,
          );
        if (!valid || (workspaceId && value.workspaceId !== workspaceId)) {
          lastGenerations = undefined;
          lastWorkspaceId = undefined;
          notify();
          return false;
        }
        if (
          (lastWorkspaceId && lastWorkspaceId !== value.workspaceId) ||
          (lastGenerations &&
            expected.some(
              (changedScope) =>
                generations[changedScope] < lastGenerations![changedScope],
            ))
        ) {
          lastGenerations = undefined;
          notify();
        }
        lastWorkspaceId = value.workspaceId;
        applyGenerations(generations!);
        if (acceptRevisions) receiveTranscriptRevisions(value);
        return true;
      };
      const broadcast = (message: Record<string, unknown>) => {
        try {
          channel?.postMessage({ protocol: 2, ...message, workspaceId });
        } catch {
          // A closed channel is treated as a lost coordinator; polling recovers it.
          releaseStreamLease();
          startFallbackPolling();
        }
      };
      const stopFallbackPolling = () => {
        if (pollTimer !== undefined) clearInterval(pollTimer);
        pollTimer = undefined;
      };
      const pollGenerations = () => {
        if (!available()) return;
        void api<{
          protocol: number;
          workspaceId: string;
          generations: Record<string, number>;
        }>("/api/sync/generations")
          .then((value) => {
            if (stopped) return;
            const accepted = applyGenerationState(value);
            if (isOwner)
              broadcast(
                accepted
                  ? { kind: "generations", ...value }
                  : { kind: "invalidate" },
              );
          })
          .catch(() => {});
      };
      const startFallbackPolling = () => {
        if (pollTimer !== undefined || stopped) return;
        pollGenerations();
        pollTimer = setInterval(pollGenerations, fallbackPollMs);
      };
      const publishHeartbeat = () => {
        if (!isOwner || stopped || !available()) return;
        broadcast({
          kind: "heartbeat",
          generations: lastGenerations,
          tokenRates: latestTokenRates,
          ...(latestTranscriptRevisions &&
          latestTranscriptRevisions.workspaceId === workspaceId &&
          latestTranscriptRevisions.revisions
            ? { transcriptRevisions: latestTranscriptRevisions.revisions }
            : {}),
        });
      };
      const closeSource = () => {
        if (source) {
          clearWorkspaceTokenRates();
          latestTokenRates = { rates: {}, teams: {} };
        }
        source?.close();
        source = undefined;
      };
      const releaseStreamLease = () => {
        isOwner = false;
        closeSource();
        const release = releaseOwnerLock;
        releaseOwnerLock = undefined;
        release?.();
        if (heartbeatTimer !== undefined) clearInterval(heartbeatTimer);
        heartbeatTimer = undefined;
        stopFallbackPolling();
      };
      const connect = () => {
        if (!available() || !isOwner || source) return;
        let connectedSource: EventSource;
        try {
          connectedSource = new EventSource("/api/sync/stream?protocol=2");
          source = connectedSource;
        } catch {
          // The elected lock holder already polls the compact generation row.
          return;
        }
        connectedSource.addEventListener("token-rates", (event) => {
          if (source !== connectedSource) return;
          try {
            const message = JSON.parse((event as MessageEvent).data);
            if (
              message.protocol === 2 &&
              message.workspaceId === workspaceId &&
              receiveWorkspaceTokenRates(message)
            ) {
              latestTokenRates = { rates: message.rates, teams: message.teams };
              broadcast({ ...message, kind: "token-rates" });
            }
          } catch {
            // Malformed telemetry cannot invalidate sync projections.
          }
        });
        connectedSource.onopen = () => {
          if (openedStream) {
            notify();
            broadcast({ kind: "invalidate" });
          }
          openedStream = true;
        };
        connectedSource.onmessage = (event) => {
          try {
            const message = JSON.parse(event.data);
            if (
              message?.protocol === 2 &&
              message.generations &&
              typeof message.generations === "object"
            ) {
              const accepted = applyGenerationState(message);
              broadcast(
                accepted
                  ? { kind: "generations", ...message }
                  : { kind: "invalidate" },
              );
              return;
            }
          } catch {
            // Legacy RESYNC data and unknown protocol values invalidate all.
          }
          lastGenerations = undefined;
          lastWorkspaceId = undefined;
          notify();
          broadcast({ kind: "invalidate" });
        };
      };
      const resume = () => {
        if (available() && isOwner) {
          // A socket can remain OPEN after mobile suspension but never deliver again.
          closeSource();
          connect();
          notify();
          broadcast({ kind: "invalidate" });
        } else if (available()) {
          notify();
          if (Date.now() - lastHeartbeatAt >= coordinatorTimeoutMs)
            startFallbackPolling();
          if (channel) startAsOwner();
        } else closeSource();
      };
      const stopResume = onResume(resume);
      const suspend = () => {
        if (!available()) {
          if (document.hidden && isOwner) releaseStreamLease();
          else closeSource();
        }
      };
      window.addEventListener("offline", suspend);
      document.addEventListener("visibilitychange", suspend);
      const startAsOwner = () => {
        if (stopped || isOwner || ownerLockPending) return;
        const locks = navigator.locks;
        if (!locks || !workspaceId) {
          startFallbackPolling();
          return;
        }
        ownerLockPending = true;
        void locks
          .request(
            `codex-sync-stream:${location.origin}:${workspaceId}`,
            { mode: "exclusive", ifAvailable: true },
            async (lock) => {
              ownerLockPending = false;
              if (stopped || !lock || !available()) {
                if (!stopped) startFallbackPolling();
                return;
              }
              isOwner = true;
              stopFallbackPolling();
              publishHeartbeat();
              heartbeatTimer = setInterval(publishHeartbeat, heartbeatMs);
              pollGenerations();
              pollTimer = setInterval(pollGenerations, fallbackPollMs);
              connect();
              await new Promise<void>((resolve) => {
                releaseOwnerLock = resolve;
              });
              releaseOwnerLock = undefined;
              isOwner = false;
              closeSource();
              if (heartbeatTimer !== undefined) clearInterval(heartbeatTimer);
              heartbeatTimer = undefined;
            },
          )
          .catch(() => {
            ownerLockPending = false;
            startFallbackPolling();
          });
      };
      const receive = (event: MessageEvent) => {
        if (stopped) return;
        const message = event.data;
        if (
          !message ||
          message.workspaceId !== workspaceId ||
          message.protocol !== 2 ||
          typeof message.kind !== "string"
        ) {
          notify();
          return;
        }
        lastHeartbeatAt = Date.now();
        stopFallbackPolling();
        if (message.kind === "token-rates") {
          receiveWorkspaceTokenRates(message);
          return;
        }
        if (message.kind === "heartbeat") {
          if (message.generations) applyGenerationState(message, false);
          if (
            message.tokenRates &&
            receiveWorkspaceTokenRates(message.tokenRates)
          )
            latestTokenRates = message.tokenRates;
          return;
        }
        if (message.kind === "invalidate") {
          lastGenerations = undefined;
          lastWorkspaceId = undefined;
          notify();
          return;
        }
        if (message.kind === "generations") applyGenerationState(message);
        else notify();
      };
      const initialize = async () => {
        try {
          const identity = await syncDatabase();
          if (stopped) return;
          workspaceId = identity.workspaceId;
          if (typeof BroadcastChannel === "undefined") {
            startFallbackPolling();
            return;
          }
          try {
            channel = new BroadcastChannel(
              `codex-sync-${location.origin}-${workspaceId}`,
            );
            channel.onmessage = receive;
          } catch {
            channel = undefined;
            startFallbackPolling();
            return;
          }
          watchdogTimer = setInterval(() => {
            if (stopped || isOwner || !available()) return;
            if (Date.now() - lastHeartbeatAt >= coordinatorTimeoutMs) {
              clearWorkspaceTokenRates();
              startFallbackPolling();
              startAsOwner();
            }
          }, heartbeatMs);
          startAsOwner();
        } catch {
          // Cached clients remain useful offline; bounded polling retries discovery.
          startFallbackPolling();
        }
      };
      stopInvalidations = () => {
        stopped = true;
        clearWorkspaceTokenRates();
        releaseStreamLease();
        clearTimeout(flushTimer);
        stopFallbackPolling();
        if (heartbeatTimer !== undefined) clearInterval(heartbeatTimer);
        if (watchdogTimer !== undefined) clearInterval(watchdogTimer);
        stopResume();
        window.removeEventListener("offline", suspend);
        document.removeEventListener("visibilitychange", suspend);
        if (channel) {
          channel.onmessage = null;
          channel.close();
          channel = undefined;
        }
      };
      notify();
      void initialize();
    }
  };
  ensureCoordinator();
  return () => {
    invalidations.delete(resync);
    if (!invalidations.size) {
      stopInvalidations?.();
      stopInvalidations = undefined;
    }
  };
}
function watchTranscriptInvalidations(id: string, resync: () => void) {
  return watchSyncInvalidations(`transcript:${id}`, resync);
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
async function persistProjectionBatch(
  collection: RxCollection<SyncDocument>,
  documents: SyncDocument[],
) {
  const pending = new Map(documents.map((row) => [row.id, row]));
  while (pending.size) {
    const existing = await collection.storageInstance.findDocumentsById(
      [...pending.keys()],
      true,
    );
    const byId = new Map(existing.map((row) => [row.id, row]));
    const writes = [];
    for (const [id, incoming] of pending) {
      const previous = byId.get(id);
      if (previous && previous.seq >= incoming.seq) {
        pending.delete(id);
        continue;
      }
      const document: RxDocumentData<SyncDocument> = {
        ...incoming,
        _deleted: incoming._deleted === true,
        _attachments: {},
        _meta: { lwt: 1 },
        _rev: "",
      };
      writes.push({ previous, document });
    }
    if (!writes.length) return;
    const result = await collection.storageInstance.bulkWrite(
      writes,
      "studio-projection-pull",
    );
    const failure = result.error.find((row) => row.status !== 409);
    if (failure) throw failure;
    if (!result.error.length) return;
  }
}
async function writeProjectionRows(
  collection: RxCollection<SyncDocument>,
  rows: Array<{
    previous?: RxDocumentData<SyncDocument>;
    document: RxDocumentData<SyncDocument>;
  }>,
) {
  if (!rows.length) return;
  const result = await collection.storageInstance.bulkWrite(
    rows,
    "studio-projection-reset",
  );
  if (result.error.length)
    throw new Error("Could not reset the local entity projection safely.");
}
async function resetEntityProjection(collection: RxCollection<SyncDocument>) {
  const markerId = "state:entities:ready";
  const found = await collection.storageInstance.findDocumentsById(
    [
      markerId,
      "state:entities:checkpoint",
      "state:entities:initial",
      "state:entities:complete",
    ],
    true,
  );
  const byId = new Map(found.map((row) => [row.id, row]));
  const marker = byId.get(markerId);
  const checkpoint = byId.get("state:entities:checkpoint");
  const initial = byId.get("state:entities:initial");
  const complete = byId.get("state:entities:complete");
  const markerSeq = Math.max(0, marker?.seq ?? 0);
  const raw = (
    id: string,
    payload: string,
    seq: number,
    deleted = false,
    previous?: RxDocumentData<SyncDocument>,
  ) => ({
    previous,
    document: {
      id,
      payload,
      seq,
      _deleted: deleted,
      _attachments: {},
      _meta: { lwt: 1 },
      _rev: "",
    } as RxDocumentData<SyncDocument>,
  });
  // Hide the current projection first. The marker stays hidden until every
  // replacement page has been stored, so observers never see a partial set.
  await writeProjectionRows(collection, [
    raw(markerId, "resetting", markerSeq + 1, false, marker),
    raw("state:entities:checkpoint", "{}", 0, false, checkpoint),
    ...(initial ? [raw("state:entities:initial", "{}", 0, true, initial)] : []),
    ...(complete
      ? [raw("state:entities:complete", "{}", 0, true, complete)]
      : []),
  ]);
  const docs = await collection
    .find({
      selector: { id: { $gte: "entity:", $lt: "entity;" } },
    })
    .exec();
  const stored = await collection.storageInstance.findDocumentsById(
    docs.map((doc: any) => doc.id),
    true,
  );
  const rows = stored.map((previous) => {
    return raw(previous.id, previous.payload, 0, true, previous);
  });
  for (let offset = 0; offset < rows.length; offset += ENTITY_BATCH_SIZE)
    await writeProjectionRows(
      collection,
      rows.slice(offset, offset + ENTITY_BATCH_SIZE),
    );
  return markerSeq + 2;
}
type TranscriptProjectionMeta = {
  format: 2;
  metadata: Record<string, any>;
  order: string[];
  itemRevisions: Record<string, string>;
};
const transcriptItemId = (scope: string, id: string) =>
  `transcript:item:${scope.slice("transcript:".length)}:${id}`;
function transcriptMetadata(value: Record<string, any>) {
  const {
    items: _items,
    delta: _delta,
    removed: _removed,
    order: _order,
    itemRevisions: _itemRevisions,
    ...metadata
  } = value;
  return metadata;
}
function readTranscriptMeta(payload: string): TranscriptProjectionMeta | null {
  try {
    const value = JSON.parse(payload);
    if (
      value?.format === 2 &&
      value.metadata &&
      Array.isArray(value.order) &&
      value.itemRevisions
    )
      return value as TranscriptProjectionMeta;
  } catch {
    /* An older client stored the full page at this key. */
  }
  return null;
}
async function cacheStoredTranscript(
  collection: RxCollection<SyncDocument>,
  workspaceId: string,
  scope: string,
  doc: SyncDocument & { _deleted?: boolean },
) {
  const id = scope.slice("transcript:".length);
  if (doc._deleted) {
    cacheTranscriptValue(workspaceId, id, null, doc.seq, 0);
    return;
  }
  if ((peekTranscript(workspaceId, id)?.seq || 0) >= doc.seq) return;
  let raw: Record<string, any>;
  try {
    raw = JSON.parse(doc.payload);
  } catch {
    return;
  }
  const meta = readTranscriptMeta(doc.payload);
  if (!meta) {
    // Read and migrate a pre-item-projection cache once.
    if (Array.isArray(raw.items)) cacheTranscript(workspaceId, id, doc);
    return;
  }
  const rows = meta.order.length
    ? await collection.storageInstance.findDocumentsById(
        meta.order.map((itemId) => transcriptItemId(scope, itemId)),
        true,
      )
    : [];
  const byId = new Map(rows.map((row) => [row.id, row]));
  const items = meta.order.flatMap((itemId) => {
    const row = byId.get(transcriptItemId(scope, itemId));
    if (!row || row._deleted) return [];
    try {
      return [JSON.parse(row.payload)];
    } catch {
      return [];
    }
  });
  cacheTranscriptValue(workspaceId, id, { ...meta.metadata, items }, doc.seq);
}
async function persistTranscriptProjection(
  collection: RxCollection<SyncDocument>,
  scope: string,
  document: SyncDocument & { _deleted?: boolean },
  previous: SyncDocument | undefined,
  workspaceId: string,
) {
  const id = scope.slice("transcript:".length);
  const base = previous?.payload ? readTranscriptMeta(previous.payload) : null;
  const legacyBase =
    previous?.payload && !base ? JSON.parse(previous.payload) : null;
  const incoming = document._deleted ? {} : JSON.parse(document.payload);
  const startingItems = base
    ? []
    : Array.isArray(legacyBase?.items)
      ? legacyBase.items
      : [];
  const oldOrder: string[] =
    base?.order ||
    startingItems
      .map((item: any) => item.id)
      .filter((key: unknown): key is string => typeof key === "string");
  const isDelta = !document._deleted && incoming.delta === true;
  if (isDelta && !base && !legacyBase?.items)
    throw new Error("Transcript delta has no local item base.");

  const changedItems = document._deleted
    ? []
    : (incoming.items || []).filter(
        (item: any) => typeof item?.id === "string",
      );
  const removed: string[] = document._deleted
    ? oldOrder
    : isDelta
      ? (incoming.removed || []).filter(
          (key: unknown) => typeof key === "string",
        )
      : [];
  const order = document._deleted
    ? []
    : Array.isArray(incoming.order)
      ? incoming.order.filter((key: unknown) => typeof key === "string")
      : isDelta
        ? oldOrder
            .filter((key) => !removed.includes(key))
            .concat(
              changedItems
                .map((item: any) => item.id)
                .filter((key: string) => !oldOrder.includes(key)),
            )
        : changedItems.map((item: any) => item.id);
  if (!document._deleted && !isDelta)
    removed.push(...oldOrder.filter((key) => !order.includes(key)));
  const previousMetadata =
    base?.metadata || transcriptMetadata(legacyBase || {});
  const metadata = document._deleted
    ? {}
    : { ...previousMetadata, ...transcriptMetadata(incoming) };
  const itemRevisions = document._deleted
    ? {}
    : { ...base?.itemRevisions, ...incoming.itemRevisions };
  for (const itemId of removed) delete itemRevisions[itemId];
  const nextMeta: TranscriptProjectionMeta = {
    format: 2,
    metadata,
    order,
    itemRevisions,
  };
  const rows: SyncDocument[] = changedItems.map((item: any) => ({
    id: transcriptItemId(scope, item.id),
    payload: JSON.stringify(item),
    seq: document.seq,
  }));
  rows.push(
    ...removed.map((itemId) => ({
      id: transcriptItemId(scope, itemId),
      payload: "{}",
      seq: document.seq,
      _deleted: true,
    })),
  );
  if (rows.length) await persistProjectionBatch(collection, rows);
  const metaDocument: SyncDocument = {
    id: scope,
    payload: JSON.stringify(nextMeta),
    seq: document.seq,
    _deleted: document._deleted,
  };
  await persistProjection(collection, metaDocument);
  if (!document._deleted && !isDelta) {
    cacheTranscriptValue(
      workspaceId,
      id,
      { ...metadata, items: changedItems },
      document.seq,
      document.payload.length * 2,
    );
  } else if (!document._deleted) {
    const cacheMetadata = transcriptMetadata(incoming);
    const patched = patchTranscriptValue(
      workspaceId,
      id,
      cacheMetadata,
      document.seq,
      changedItems,
      removed,
      Array.isArray(incoming.order) ? order : undefined,
    );
    if (!patched)
      await cacheStoredTranscript(collection, workspaceId, scope, metaDocument);
  } else {
    cacheTranscriptValue(workspaceId, id, null, document.seq, 0);
  }
}
type ProjectionState = {
  users: number;
  foreground: number;
  stop: () => Promise<unknown>;
  refresh: () => Promise<void>;
  activateInvalidation: () => void;
  listeners: Set<(error: unknown | null) => void>;
};
const scopes = new Map<string, ProjectionState>();
const closingScopes = new Map<string, Promise<unknown>>();
const ENTITY_BATCH_SIZE = 500;
// Projection refresh retries only read-only pulls, never send or draft writes.
const syncReadRetryBaseMs = 250;
const syncReadRetryMaxMs = 8000;
const syncReadRetryMaxExponent = 5;
const isTransientSyncReadFailure = (error: unknown) =>
  error instanceof TypeError ||
  (error instanceof ApiError &&
    ((error.status >= 500 && error.status < 600) ||
      [408, 429].includes(error.status)));
async function acquireProjection(
  scope: string,
  expectedWorkspace?: string,
  background = false,
) {
  const { db, workspaceId, verifyWorkspace } = await syncDatabase();
  if (expectedWorkspace && expectedWorkspace !== workspaceId)
    throw new WorkspaceMismatchError();
  while (closingScopes.has(scope)) await closingScopes.get(scope);
  const remoteScope = scope === "state" ? "state:entities:v1" : scope;
  let state = scopes.get(scope);
  if (!state) {
    const listeners = new Set<(error: unknown | null) => void>();
    let stopped = false;
    let pending: Promise<void> | undefined;
    let invalidated = false;
    let resetReadySeq: number | undefined;
    let retryTimer: ReturnType<typeof setTimeout> | undefined;
    let retryAttempt = 0;
    let retryableError: unknown;
    let invalidationBlocked = false;
    const retryAvailable = () => !document.hidden && navigator.onLine !== false;
    const clearRetry = () => {
      if (retryTimer !== undefined) clearTimeout(retryTimer);
      retryTimer = undefined;
    };
    const retry = () => {
      if (
        stopped ||
        retryTimer !== undefined ||
        retryableError === undefined ||
        !retryAvailable()
      )
        return;
      const exponent = Math.min(retryAttempt, syncReadRetryMaxExponent);
      const delay = Math.min(
        syncReadRetryBaseMs * 2 ** exponent,
        syncReadRetryMaxMs,
      );
      retryAttempt++;
      retryTimer = setTimeout(() => {
        retryTimer = undefined;
        if (retryAvailable()) void refresh().catch(() => {});
      }, delay);
    };
    const retryWhenAvailable = () => {
      if (retryableError !== undefined && retryAvailable()) retry();
    };
    window.addEventListener("online", retryWhenAvailable);
    document.addEventListener("visibilitychange", retryWhenAvailable);
    const report = (error: unknown | null) => {
      invalidationBlocked =
        error instanceof WorkspaceMismatchError ||
        (error instanceof ApiError && !isTransientSyncReadFailure(error));
      if (error === null) {
        retryableError = undefined;
        retryAttempt = 0;
        clearRetry();
      }
      listeners.forEach((listener) => listener(error));
    };

    const checkpointId =
      remoteScope === "state:entities:v1" ? "state:entities:checkpoint" : scope;
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
          let more = true;
          const markers =
            remoteScope === "state:entities:v1"
              ? await db.projections.storageInstance.findDocumentsById(
                  [
                    "state:entities:ready",
                    "state:entities:initial",
                    "state:entities:complete",
                  ],
                  true,
                )
              : [];
          const ready = markers.find(
            (row) =>
              row.id === "state:entities:ready" &&
              !row._deleted &&
              row.payload === "ready",
          );
          const initialMarker = markers.find(
            (row) => row.id === "state:entities:initial" && !row._deleted,
          );
          const readyMarker = markers.find(
            (row) => row.id === "state:entities:ready",
          );
          let complete = markers.find(
            (row) => row.id === "state:entities:complete" && !row._deleted,
          );
          if (
            readyMarker?.payload === "resetting" &&
            resetReadySeq === undefined
          ) {
            resetReadySeq = await resetEntityProjection(db.projections);
            complete = undefined;
          }
          let readyPublished = !!ready;
          let initialHigh: number | undefined =
            remoteScope === "state:entities:v1" && !complete
              ? (initialMarker?.seq ?? 0)
              : undefined;
          while (more && !stopped) {
            const [previous] =
              await db.projections.storageInstance.findDocumentsById(
                [checkpointId],
                true,
              );
            const after = previous?.seq ?? 0;
            if (initialHigh !== undefined && previous?.payload) {
              try {
                initialHigh =
                  initialMarker?.seq ??
                  (JSON.parse(previous.payload).initialHigh || 0);
              } catch {
                /* An old checkpoint continues with delta semantics. */
                initialHigh = undefined;
              }
            }
            const result = await pull(
              remoteScope,
              after,
              remoteScope === "state:entities:v1" ? ENTITY_BATCH_SIZE : 100,
              workspaceId,
              verifyWorkspace,
              initialHigh,
              remoteScope === "state:entities:v1" &&
                after === 0 &&
                initialHigh !== undefined &&
                window.matchMedia("(max-width: 760px)").matches
                ? saved<string | null>("codex-mobile-opened", null)
                : null,
            );
            if (stopped) return;
            if (remoteScope === "state:entities:v1" && result.reset === true) {
              resetReadySeq = await resetEntityProjection(db.projections);
              initialHigh = 0;
              continue;
            }
            if (
              remoteScope === "state:entities:v1" &&
              initialHigh !== undefined
            )
              await persistProjection(db.projections, {
                id: "state:entities:initial",
                payload: "{}",
                seq: result.initialHigh,
              });
            const entityBatch: SyncDocument[] = [];
            for (const document of result.documents as SyncDocument[]) {
              if (
                remoteScope === "state:entities:v1"
                  ? !document.id.startsWith("entity:")
                  : document.id !== remoteScope
              )
                throw new Error(
                  "The server returned an invalid projection document.",
                );
              if (remoteScope === "state:entities:v1") {
                entityBatch.push(document);
                continue;
              }
              if (scope.startsWith("transcript:")) {
                let transcriptDocument = document;
                if (!document._deleted) {
                  const incoming = JSON.parse(document.payload);
                  if (
                    incoming.delta &&
                    (!previous?.payload ||
                      !readTranscriptMeta(previous.payload))
                  ) {
                    const full = await pull(
                      scope,
                      0,
                      100,
                      workspaceId,
                      verifyWorkspace,
                    );
                    const fullDocument = full.documents?.find(
                      (row: SyncDocument) => row.id === scope && !row._deleted,
                    );
                    if (!fullDocument)
                      throw new Error(
                        "Transcript delta has no local item base and the full pull returned no snapshot.",
                      );
                    const fullPayload = JSON.parse(fullDocument.payload);
                    if (
                      fullPayload.delta === true ||
                      !Array.isArray(fullPayload.items)
                    )
                      throw new Error(
                        "Transcript delta has no local item base and the full pull returned an invalid snapshot.",
                      );
                    transcriptDocument = fullDocument;
                  }
                }
                await persistTranscriptProjection(
                  db.projections,
                  scope,
                  transcriptDocument,
                  previous,
                  workspaceId,
                );
                continue;
              }
              await persistProjection(db.projections, {
                ...document,
                id: scope,
              });
            }
            if (remoteScope === "state:entities:v1") {
              await persistProjectionBatch(db.projections, entityBatch);
              await persistProjection(db.projections, {
                id: checkpointId,
                payload: JSON.stringify({ initialHigh: result.initialHigh }),
                seq: result.checkpoint.seq,
              });
              if (!readyPublished) {
                await persistProjection(db.projections, {
                  id: "state:entities:ready",
                  payload: "ready",
                  seq: 1,
                });
                readyPublished = true;
              }
              initialHigh = result.initialHigh;
              more =
                result.documents.length === ENTITY_BATCH_SIZE &&
                result.checkpoint.seq < result.maxSeq;
            } else {
              more = false;
            }
          }
          report(null);
        } while (invalidated && !stopped);
        if (!stopped && remoteScope === "state:entities:v1") {
          await persistProjection(db.projections, {
            id: "state:entities:complete",
            payload: "complete",
            seq: Date.now(),
          });
          await persistProjection(db.projections, {
            id: "state:entities:ready",
            payload: "ready",
            seq: resetReadySeq ?? 1,
          });
        }
      })()
        .catch((error) => {
          retryableError = isTransientSyncReadFailure(error)
            ? error
            : undefined;
          report(error);
          throw error;
        })
        .finally(() => {
          pending = undefined;
          retry();
        });
      return pending;
    };
    const transcriptId = scope.startsWith("transcript:")
      ? scope.slice(11)
      : null;
    let stopInvalidation: (() => void) | undefined;
    const invalidate = () => {
      // A stream hint cannot authorize another read after a permanent rejection.
      // Explicit refresh still uses the normal workspace and HTTP checks.
      if (invalidationBlocked) return;
      invalidated = true;
      void refresh().catch(() => {});
    };
    const activateInvalidation = () => {
      if (stopInvalidation) return;
      stopInvalidation = transcriptId
        ? watchTranscriptInvalidations(transcriptId, invalidate)
        : watchSyncInvalidations(
            invalidate,
            remoteScope === "state:entities:v1" ? "entities" : "legacy",
          );
    };
    if (!background || !transcriptId) activateInvalidation();
    state = {
      users: 0,
      foreground: 0,
      listeners,
      refresh,
      activateInvalidation,
      stop: async () => {
        stopped = true;
        clearRetry();
        window.removeEventListener("online", retryWhenAvailable);
        document.removeEventListener("visibilitychange", retryWhenAvailable);
        stopInvalidation?.();
        await pending?.catch(() => {});
      },
    };
    scopes.set(scope, state);
  }
  state.users++;
  if (!background) {
    state.foreground++;
    state.activateInvalidation();
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
      if (doc)
        void cacheStoredTranscript(db.projections, workspaceId, scope, doc);
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
  const subscription =
    scope === "state"
      ? (() => {
          const projection = emptyEntityProjection();
          let ready = false;
          let entitiesLoaded = false;
          let latestRows: any[] = [];
          const publishCurrent = () => {
            if (!ready || !entitiesLoaded) return;
            const next = applyEntityRows(projection, latestRows, true);
            if (next) accept(next);
          };
          const publish = (documents: any[]) => {
            entitiesLoaded = true;
            latestRows = documents.map((document) =>
              document.toJSON ? document.toJSON() : document,
            );
            publishCurrent();
          };
          const entities = db.projections
            .find({ selector: { id: { $gte: "entity:", $lt: "entity;" } } })
            .$.subscribe(publish);
          const marker = db.projections
            .findOne("state:entities:ready")
            .$.subscribe((document: any) => {
              const row = document?.toJSON ? document.toJSON() : document;
              ready = row?.payload === "ready" && !row?._deleted;
              publishCurrent();
            });
          return {
            unsubscribe() {
              entities.unsubscribe();
              marker.unsubscribe();
            },
          };
        })()
      : db.projections.findOne(scope).$.subscribe((doc: any) => {
          if (!id) {
            accept(doc ? JSON.parse(doc.payload) : null);
            return;
          }
          if (doc)
            void cacheStoredTranscript(
              db.projections,
              workspaceId,
              `transcript:${id}`,
              doc,
            );
          else if (!peekTranscript(workspaceId, id)) accept(null);
        });
  return () => {
    subscription.unsubscribe();
    stopCache();
    state.listeners.delete(fail);
    void release();
  };
}

/** Reconcile the entity projection after an action without fetching /api/state. */
export async function refreshProjection(scope: string) {
  const handle = await acquireProjection(scope);
  try {
    await handle.state.refresh();
  } finally {
    await handle.release();
  }
}

export const TRANSCRIPT_PREFETCH_LIMIT = 1;
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
  if (prefetches >= TRANSCRIPT_PREFETCH_LIMIT) return Promise.resolve(false);
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
          if (doc)
            void cacheStoredTranscript(
              handle.db.projections,
              workspaceId,
              `transcript:${id}`,
              doc,
            );
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
  const stopInvalidation = watchSyncInvalidations(
    () => replication.reSync(),
    "drafts",
  );
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
      if (!stopped) {
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
