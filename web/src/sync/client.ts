import { createRxDatabase, addRxPlugin } from "rxdb";
import { getRxStorageDexie } from "rxdb/plugins/storage-dexie";
import { RxDBLeaderElectionPlugin } from "rxdb/plugins/leader-election";
import { replicateRxCollection } from "rxdb/plugins/replication";
import type { RxCollection, RxDocument, RxDocumentData } from "rxdb";
import { draftConflictHandler } from "./conflicts";
import {
  applyEntityRows,
  emptyEntityProjection,
  type EntityRow,
} from "./entityProjection";
import {
  syncGet,
  syncPost,
  ApiError,
  saved,
  save,
  setWorkspace,
  registerSyncEntityPersister,
  type GetResult,
} from "../api";
import type { Snapshot } from "../types";
import { EntitySequenceCheckpoint } from "./entitySequence";
import {
  acknowledgeEntitySequences,
  watchResourceChanges,
  watchResourceConnection,
  type ResourceConnectionState,
  type ResourceRef,
  type ResourceVersion,
} from "./resourceEvents";

import { onResume } from "./resume";
import { readRetryDelay, retryableReadError } from "./readRetry";
import { refreshAfterCurrentPull } from "./refreshAfterCurrentPull";
import { isEntityResetResponse, requiredSyncNumber } from "./pullContract";
import {
  cacheTranscriptValue,
  peekTranscript,
  patchTranscriptValue,
  subscribeTranscript,
} from "./transcriptCache";
import { DRAFT_SYNC_TIMING_MS } from "./draftSyncTiming.mjs";

export { DRAFT_SYNC_TIMING_MS } from "./draftSyncTiming.mjs";

addRxPlugin(RxDBLeaderElectionPlugin);
const DRAFT_PUSH_QUIET_WAIT_MS = DRAFT_SYNC_TIMING_MS.pushQuietWait;
const DRAFT_PUSH_MAX_WAIT_MS = DRAFT_SYNC_TIMING_MS.pushMaxWait;
const DRAFT_PUSH_RETRY_TIME_MS = DRAFT_SYNC_TIMING_MS.pushRetry;
// Failed pushes already retry this often; restart triggers must not accelerate them.
const DRAFT_PUSH_RESTART_MIN_INTERVAL_MS =
  DRAFT_SYNC_TIMING_MS.restartMinInterval;
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
  const identify = async () => {
    if (navigator.onLine === false)
      throw new TypeError("The device is offline.");
    const identity = await syncGet("/api/sync/identity");
    if (!/^[a-f0-9]{32}$/.test(identity.workspaceId))
      throw new Error("Invalid workspace identity");
    return identity;
  };
  const initialIdentity = identify();
  let grace: ReturnType<typeof setTimeout> | undefined;
  let identity: Awaited<ReturnType<typeof identify>> | null = null;
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
  let firstCheck: ReturnType<typeof identify> | undefined = initialIdentity;
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
  registerSyncEntityPersister(async (targetWorkspaceId, documents) => {
    const { db, workspaceId } = await syncDatabase();
    if (targetWorkspaceId && targetWorkspaceId !== workspaceId) return;
    const entities = documents.filter((document) =>
      document.id.startsWith("entity:"),
    );
    // Acknowledge the response envelope before the next coalesced state frame
    // can schedule a redundant pull; a failed persistence triggers reconciliation.
    acknowledgeEntitySequences(
      entities.map((document) => document.seq),
      targetWorkspaceId,
    );
    try {
      for (const document of entities)
        await persistProjection(db.projections, document);
      const stateProjection = scopes.get("state");
      const sequences = entities.map((document) => document.seq);
      const [checkpointBefore] =
        await db.projections.storageInstance.findDocumentsById(
          ["state:entities:checkpoint"],
          true,
        );
      await stateProjection?.acknowledgeEntitySequences(sequences);
      const [checkpointAfter] =
        await db.projections.storageInstance.findDocumentsById(
          ["state:entities:checkpoint"],
          true,
        );
      // Precise browser-test diagnostic: distinguishes the completed renderer
      // persister from the earlier HTTP response and UI selection transition.
      performance.mark("studio-sync-entity-persister-done", {
        detail: {
          sequences,
          checkpointBefore: checkpointBefore?.seq ?? null,
          checkpointAfter: checkpointAfter?.seq ?? null,
          activeProjection: !!stateProjection,
        },
      });
    } catch (error) {
      await refreshProjection("state:entities:v1").catch(() => {});
      throw error;
    }
  });

async function pull(
  scope: string,
  after: number,
  limit: number,
  workspaceId: string,
  verifyWorkspace: () => Promise<void>,
  initialHigh?: number,
  priorityId?: string | null,
  signal?: AbortSignal,
) {
  if (signal?.aborted) throw new DOMException("Aborted", "AbortError");
  await verifyWorkspace();
  const result = await syncGet("/api/sync/pull", {
    query: {
      scope,
      after,
      limit,
      ...(initialHigh !== undefined ? { fresh: "1", initialHigh } : {}),
      ...(scope === "state:entities:v1" ? { reset: "1" } : {}),
      ...(priorityId ? { priorityId } : {}),
    },
    ...(signal ? { signal } : {}),
  });
  if (result.workspaceId !== workspaceId) throw new WorkspaceMismatchError();
  return result;
}
export {
  watchResourceChanges,
  watchResourceConnection,
  type ResourceConnectionState,
} from "./resourceEvents";
export type { ResourceRef } from "./resourceEvents";

function resourceForScope(scope: string): ResourceRef {
  if (scope === "drafts") return { kind: "drafts" };
  if (scope.startsWith("transcript:"))
    return { kind: "transcript", agentId: scope.slice("transcript:".length) };
  return { kind: "state" };
}

export function watchSyncInvalidations(
  resync: (version?: ResourceVersion) => void,
  scope?: string,
): () => void;
export function watchSyncInvalidations(
  scope: string,
  resync: (version?: ResourceVersion) => void,
): () => void;
export function watchSyncInvalidations(
  first: string | (() => void),
  second: string | (() => void) = "legacy",
) {
  const scope = typeof first === "string" ? first : String(second);
  const resync = typeof first === "function" ? first : second;
  if (typeof resync !== "function") return () => {};
  return watchResourceChanges(resourceForScope(scope), resync);
}
function watchTranscriptInvalidations(id: string, resync: () => void) {
  return watchResourceChanges({ kind: "transcript", agentId: id }, resync);
}
// Pull-only projections never run RxDB's upstream scan of every cached chat.
// The storage revision is a compare-and-swap across tabs; retry conflicts against
// the latest row before applying server sequence order, including tombstones.
export async function persistProjection(
  collection: RxCollection<SyncDocument>,
  incoming: SyncDocument,
  replaceObsoleteTranscript = false,
) {
  for (;;) {
    const [previous] = await collection.storageInstance.findDocumentsById(
      [incoming.id],
      true,
    );
    if (previous && previous.seq >= incoming.seq && !replaceObsoleteTranscript)
      return;
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
    docs.map((doc) => doc.id),
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
    return null;
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
  const meta = readTranscriptMeta(doc.payload);
  if (!meta) return;
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
  const incoming = document._deleted ? {} : JSON.parse(document.payload);
  const oldOrder: string[] = base?.order || [];
  const isDelta = !document._deleted && incoming.delta === true;
  if (isDelta && !base)
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
  const previousMetadata = base?.metadata || {};
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
  await persistProjection(
    collection,
    metaDocument,
    !document._deleted && !isDelta && !!previous && !base,
  );
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
  refresh: (signal?: AbortSignal) => Promise<void>;
  refreshAfterCurrent: () => Promise<void>;
  activateInvalidation: () => void;
  acknowledgeEntitySequences: (sequences: number[]) => Promise<void>;
  listeners: Set<(error: unknown | null) => void>;
};
const scopes = new Map<string, ProjectionState>();
const closingScopes = new Map<string, Promise<unknown>>();
const ENTITY_BATCH_SIZE = 500;
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
    let unversionedInvalidation = false;
    let requiredEntitySequence: number | undefined;
    // Sequence zero is a valid initial baseline. Keep a sentinel until the
    // first pull establishes that the local projection already has that row.
    const latestEntitySequence = new EntitySequenceCheckpoint();
    if (remoteScope === "state:entities:v1") {
      const [checkpoint] =
        await db.projections.storageInstance.findDocumentsById(
          ["state:entities:checkpoint"],
          true,
        );
      if (checkpoint) latestEntitySequence.assign(checkpoint.seq);
    }
    let resetReadySeq: number | undefined;
    let invalidationBlocked = false;
    let readFailed = false;
    let connectionStatus: ResourceConnectionState = "connecting";
    let stopConnection: (() => void) | undefined;
    const report = (error: unknown | null) => {
      invalidationBlocked =
        error instanceof WorkspaceMismatchError ||
        (error instanceof ApiError && !isTransientSyncReadFailure(error));
      if (error === null) readFailed = false;
      else readFailed = isTransientSyncReadFailure(error);
      listeners.forEach((listener) => listener(error));
    };
    const retryRead = async <T>(
      request: () => Promise<T>,
      signal?: AbortSignal,
    ): Promise<T> => {
      for (let attempt = 0; ; attempt++) {
        try {
          return await request();
        } catch (error) {
          const canRetry =
            attempt < 2 &&
            isTransientSyncReadFailure(error) &&
            connectionStatus === "live" &&
            !document.hidden &&
            navigator.onLine !== false &&
            !signal?.aborted;
          if (!canRetry) throw error;
          const delay = 200 * 2 ** attempt + Math.random() * 150;
          await new Promise<void>((resolve, reject) => {
            const timer = setTimeout(() => {
              signal?.removeEventListener("abort", abort);
              resolve();
            }, delay);
            const abort = () => {
              clearTimeout(timer);
              reject(new DOMException("Aborted", "AbortError"));
            };
            signal?.addEventListener("abort", abort, { once: true });
          });
          if (
            connectionStatus !== "live" ||
            document.hidden ||
            navigator.onLine === false
          )
            throw error;
        }
      }
    };

    const checkpointId =
      remoteScope === "state:entities:v1" ? "state:entities:checkpoint" : scope;
    const refresh = (signal?: AbortSignal): Promise<void> => {
      if (stopped) return Promise.resolve();
      if (pending) return pending;
      pending = (async () => {
        do {
          if (signal?.aborted && scopes.get(scope)?.foreground === 0)
            throw new DOMException("Aborted", "AbortError");
          invalidated = false;
          unversionedInvalidation = false;
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
            const hasTranscriptBase =
              !scope.startsWith("transcript:") ||
              (!!previous && !!readTranscriptMeta(previous.payload));
            const after = hasTranscriptBase ? (previous?.seq ?? 0) : 0;
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
            const requestSignal =
              signal && scopes.get(scope)?.foreground === 0
                ? signal
                : undefined;
            const pullResetVersion = latestEntitySequence.resetVersion;
            const result = await retryRead(
              () =>
                pull(
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
                  requestSignal,
                ),
              requestSignal,
            );
            if (stopped) return;
            // An epoch/reset during this request invalidates the old server
            // checkpoint. Same-epoch responses remain usable and cannot move
            // the local checkpoint backwards.
            if (
              remoteScope === "state:entities:v1" &&
              !latestEntitySequence.isSameResetVersion(pullResetVersion)
            ) {
              more = true;
              continue;
            }
            if (isEntityResetResponse(result, remoteScope)) {
              latestEntitySequence.reset();
              resetReadySeq = await resetEntityProjection(db.projections);
              initialHigh = 0;
              continue;
            }
            if (
              remoteScope === "state:entities:v1" &&
              initialHigh !== undefined &&
              result.initialHigh == null
            )
              throw new Error(
                "The server omitted the initial sync checkpoint.",
              );
            if (remoteScope === "state:entities:v1" && result.maxSeq == null)
              throw new Error("The server omitted the sync sequence limit.");
            if (
              remoteScope === "state:entities:v1" &&
              initialHigh !== undefined
            )
              await persistProjection(db.projections, {
                id: "state:entities:initial",
                payload: "{}",
                seq: requiredSyncNumber(
                  result.initialHigh,
                  "initial checkpoint",
                ),
              });
            const entityBatch: SyncDocument[] = [];
            for (const document of result.documents) {
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
                    const full = await retryRead(
                      () =>
                        pull(
                          scope,
                          0,
                          100,
                          workspaceId,
                          verifyWorkspace,
                          undefined,
                          undefined,
                          requestSignal,
                        ),
                      requestSignal,
                    );
                    if (full.reset === true)
                      throw new Error(
                        "The server reset transcript sync unexpectedly.",
                      );
                    const fullDocument = full.documents.find(
                      (row) => row.id === scope && !row._deleted,
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
              if (!latestEntitySequence.isSameResetVersion(pullResetVersion)) {
                more = true;
                continue;
              }
              latestEntitySequence.assignWithinEpoch(result.checkpoint.seq);
              await persistProjection(db.projections, {
                id: checkpointId,
                payload: JSON.stringify({ initialHigh: result.initialHigh }),
                seq: latestEntitySequence.value,
              });
              if (!readyPublished) {
                await persistProjection(db.projections, {
                  id: "state:entities:ready",
                  payload: "ready",
                  seq: 1,
                });
                readyPublished = true;
              }
              initialHigh = requiredSyncNumber(
                result.initialHigh,
                "initial checkpoint",
              );
              more =
                result.documents.length === ENTITY_BATCH_SIZE &&
                result.checkpoint.seq <
                  requiredSyncNumber(result.maxSeq, "maximum sequence");
            } else {
              more = false;
            }
          }
          if (
            invalidated &&
            !unversionedInvalidation &&
            requiredEntitySequence !== undefined &&
            latestEntitySequence.covers(requiredEntitySequence)
          ) {
            invalidated = false;
            requiredEntitySequence = undefined;
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
          report(error);
          throw error;
        })
        .finally(() => {
          pending = undefined;
          // An invalidation can land between the final loop condition and
          // clearing `pending`; make that edge schedule one more projection.
          if (invalidated && !stopped)
            queueMicrotask(() => void refresh().catch(() => {}));
        });
      return pending;
    };
    const refreshAfterCurrent = () => refreshAfterCurrentPull(pending, refresh);
    const transcriptId = scope.startsWith("transcript:")
      ? scope.slice(11)
      : null;
    let stopInvalidation: (() => void) | undefined;
    const invalidate = (version?: ResourceVersion) => {
      // A stream hint cannot authorize another read after a permanent rejection.
      // Explicit refresh still uses the normal workspace and HTTP checks.
      if (invalidationBlocked) return;
      if (remoteScope === "state:entities:v1" && version) {
        latestEntitySequence.observeEpoch(version.epoch);
        if (version.entitySequenceReset) latestEntitySequence.reset();
      }
      if (
        remoteScope === "state:entities:v1" &&
        version?.entitySequence !== undefined
      ) {
        if (latestEntitySequence.covers(version.entitySequence)) return;
        requiredEntitySequence = Math.max(
          requiredEntitySequence ?? 0,
          version.entitySequence,
        );
      } else {
        unversionedInvalidation = true;
      }
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
      stopConnection = watchResourceConnection((status) => {
        connectionStatus = status;
        if (status === "live" && readFailed && !invalidationBlocked)
          void refresh().catch(() => {});
      });
      stopResume = onResume(() => {
        if (readFailed && !invalidationBlocked) void refresh().catch(() => {});
      });
    };
    let stopResume: (() => void) | undefined;
    if (!background || !transcriptId) activateInvalidation();
    state = {
      users: 0,
      foreground: 0,
      listeners,
      refresh,
      refreshAfterCurrent,
      activateInvalidation,
      acknowledgeEntitySequences: async (sequences) => {
        // Mutation response rows are part of the local projection. Advance
        // the server cursor only when they exactly fill the next sequence span;
        // otherwise the next pull must retrieve the gap before skipping ahead.
        const advanced = latestEntitySequence.advanceContiguous(sequences);
        if (!advanced) return;
        const [checkpoint] =
          await db.projections.storageInstance.findDocumentsById(
            [checkpointId],
            true,
          );
        await persistProjection(db.projections, {
          id: checkpointId,
          payload: checkpoint?.payload ?? JSON.stringify({ initialHigh: 0 }),
          seq: latestEntitySequence.value,
        });
      },
      stop: async () => {
        stopped = true;
        stopInvalidation?.();
        stopConnection?.();
        stopResume?.();
        await pending?.catch(() => {});
      },
    };
    scopes.set(scope, state);
  }
  state.users++;
  if (!background) {
    state.foreground++;
    state.activateInvalidation();
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

type StateProjectionPayload = Snapshot | null;
type TranscriptProjectionPayload = GetResult<"/api/transcript"> | null;

async function watchStateProjection(
  accept: (payload: StateProjectionPayload) => void,
  fail: (e: unknown) => void,
) {
  const { db, state, release } = await acquireProjection("state");
  state.listeners.add(fail);
  const projection = emptyEntityProjection();
  let ready = false;
  let entitiesLoaded = false;
  let latestRows: EntityRow[] = [];
  const publishCurrent = () => {
    if (!ready || !entitiesLoaded) return;
    const next = applyEntityRows(projection, latestRows, true);
    if (next) accept(next);
  };
  const publish = (documents: RxDocument<SyncDocument>[]) => {
    entitiesLoaded = true;
    latestRows = documents.map((document) => document.toJSON());
    publishCurrent();
  };
  const entities = db.projections
    .find({ selector: { id: { $gte: "entity:", $lt: "entity;" } } })
    .$.subscribe(publish);
  const marker = db.projections
    .findOne("state:entities:ready")
    .$.subscribe((document) => {
      const row = document?.toJSON();
      ready = row?.payload === "ready" && !row?._deleted;
      publishCurrent();
    });
  return () => {
    entities.unsubscribe();
    marker.unsubscribe();
    state.listeners.delete(fail);
    void release();
  };
}

async function watchTranscriptProjection(
  scope: `transcript:${string}`,
  accept: (payload: TranscriptProjectionPayload) => void,
  fail: (e: unknown) => void,
) {
  const { db, workspaceId, state, release } = await acquireProjection(scope);
  state.listeners.add(fail);
  const id = scope.slice("transcript:".length);
  const stopCache = subscribeTranscript(workspaceId, id, (entry) =>
    accept(entry.payload),
  );
  const subscription = db.projections.findOne(scope).$.subscribe((doc) => {
    if (doc)
      void cacheStoredTranscript(db.projections, workspaceId, scope, doc);
    else if (!peekTranscript(workspaceId, id)) accept(null);
  });
  return () => {
    subscription.unsubscribe();
    stopCache();
    state.listeners.delete(fail);
    void release();
  };
}

/** Pull and return the current entity projection for state reconciliation. */
export async function refreshProjection(
  options: { afterCurrentPull?: boolean } = {},
): Promise<Snapshot> {
  const handle = await acquireProjection("state");
  try {
    if (options.afterCurrentPull) await handle.state.refreshAfterCurrent();
    else await handle.state.refresh();
    const [ready, documents] = await Promise.all([
      handle.db.projections.findOne("state:entities:ready").exec(),
      handle.db.projections
        .find({ selector: { id: { $gte: "entity:", $lt: "entity;" } } })
        .exec(),
    ]);
    if (!ready || ready.payload !== "ready" || ready._deleted)
      throw new Error("The entity projection is not ready yet.");
    const snapshot = applyEntityRows(
      emptyEntityProjection(),
      documents.map((document) => document.toJSON()),
      true,
    );
    if (!snapshot) throw new Error("The entity projection is unavailable.");
    return snapshot;
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
  signal?: AbortSignal,
): Promise<boolean> {
  if (signal?.aborted) return Promise.resolve(false);
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
        .$.subscribe((doc) => {
          if (doc)
            void cacheStoredTranscript(
              handle.db.projections,
              workspaceId,
              `transcript:${id}`,
              doc,
            );
        });
      stopCache = () => subscription.unsubscribe();
      await handle.state.refresh(signal);
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
  report: (
    e: unknown | null,
    direction: "pull" | "push" | "replication" | null,
  ) => void,
  /** @internal Test-only instrumentation; production callers omit this field. */
  options: {
    testOnly?: {
      cancel?: (cancel: () => Promise<void>) => Promise<void>;
      onCreate?: () => void;
    };
  } = {},
) {
  const { db, workspaceId, verifyWorkspace } = await syncDatabase();
  // State machine: each invalidation sets `pullTriggered`. The first admitted
  // page consumes that bit; a successful full page permits continuation via
  // `pullContinues`, without consuming any trigger that arrived meanwhile.
  // `pullInFlight` covers the complete RxDB downstream sequence, including the
  // short gap between its page handlers, and clears only when active.down ends.
  // A failed sequence has no automatic retry; pending work remains queued, or
  // the next event sets it. With a healthy push, pending work calls reSync. With
  // a failed push, it waits until no page sequence is active, then restarts at
  // the monotonic minimum-interval deadline. Stop wakes capped startup and
  // downstream waits before cancelling the one active RxDB state.
  // Invariants: no pull sequence without a trigger; no outage trigger is lost;
  // at most one restart per minimum interval.
  const failures = new Map<"pull" | "push" | "replication", unknown>();
  let stopped = false;
  let pushFailed = false;
  let pullTriggered = false;
  let pullInFlight = false;
  let pullContinues = false;
  let replication: ReturnType<
    typeof replicateRxCollection<SyncDocument, { seq: number }>
  >;
  let replicationErrors: { unsubscribe: () => void }[] = [];
  let pullActivitySubscription: { unsubscribe: () => void } | undefined;
  let restarting: Promise<void> | undefined;
  let restartTimer: ReturnType<typeof setTimeout> | undefined;
  let lastRestartAt = Number.NEGATIVE_INFINITY;
  let replicationGeneration = 0;
  const stopWaiters = new Set<() => void>();
  const advanceReplicationGeneration = () => ++replicationGeneration;
  const waitForStop = () => {
    let cancel!: () => void;
    const promise = new Promise<void>((resolve) => {
      const finish = () => {
        stopWaiters.delete(finish);
        resolve();
      };
      cancel = finish;
      if (stopped) finish();
      else stopWaiters.add(finish);
    });
    return { promise, cancel };
  };
  const raceStopWithCap = async <T>(
    promise: Promise<T>,
    afterWait: () => void = () => {},
  ) => {
    const stop = waitForStop();
    let timeout: ReturnType<typeof setTimeout> | undefined;
    const cap = new Promise<"timeout">((resolve) => {
      timeout = setTimeout(
        () => resolve("timeout"),
        DRAFT_SYNC_TIMING_MS.restartPullWaitCap,
      );
    });
    try {
      return await Promise.race([promise, cap, stop.promise]);
    } finally {
      clearTimeout(timeout);
      stop.cancel();
      afterWait();
    }
  };
  const cancelReplication = (current: typeof replication) =>
    options.testOnly?.cancel
      ? options.testOnly.cancel(() => current.cancel())
      : current.cancel();
  const state = (
    direction: "pull" | "push" | "replication",
    error: unknown | null,
  ) => {
    if (stopped) return;
    if (error === null) failures.delete(direction);
    else failures.set(direction, error);
    if (direction === "push") {
      pushFailed = error !== null;
      if (!pushFailed) {
        clearTimeout(restartTimer);
        restartTimer = undefined;
      }
      servePendingPull();
    }
    const failedDirection = failures.has("push")
      ? "push"
      : failures.has("pull")
        ? "pull"
        : failures.has("replication")
          ? "replication"
          : null;
    report(
      failedDirection === null ? null : failures.get(failedDirection)!,
      failedDirection,
    );
  };
  const beginTriggeredPull = () => {
    if (
      stopped ||
      (pullInFlight && !pullContinues) ||
      !(pullTriggered || pullContinues)
    )
      return false;
    const continuation = pullContinues;
    pullContinues = false;
    // There is no await between admission and marking the pull active. Events
    // received after this point remain pending for a later sequence. A full
    // page continuation belongs to the admitted sequence and consumes no bit.
    if (!continuation) {
      pullTriggered = false;
      pullInFlight = true;
    }
    return true;
  };
  let pendingPushWait:
    | {
        promise: Promise<void>;
        resolve: () => void;
        quietTimer?: ReturnType<typeof setTimeout>;
        maxTimer?: ReturnType<typeof setTimeout>;
      }
    | undefined;
  const settlePushWait = (wait = pendingPushWait) => {
    if (!wait || pendingPushWait !== wait) return;
    pendingPushWait = undefined;
    clearTimeout(wait.quietTimer);
    clearTimeout(wait.maxTimer);
    wait.resolve();
  };
  const waitBeforePersist = () => {
    let wait = pendingPushWait;
    if (!wait) {
      let resolve!: () => void;
      const promise = new Promise<void>((done) => {
        resolve = done;
      });
      wait = { promise, resolve };
      pendingPushWait = wait;
      wait.maxTimer = setTimeout(
        () => settlePushWait(wait),
        DRAFT_PUSH_MAX_WAIT_MS,
      );
    } else {
      clearTimeout(wait.quietTimer);
    }
    wait.quietTimer = setTimeout(
      () => settlePushWait(wait),
      DRAFT_PUSH_QUIET_WAIT_MS,
    );
    return wait.promise;
  };
  const attempt = async <T>(
    generation: number,
    direction: "pull" | "push",
    request: () => Promise<T>,
  ) => {
    try {
      const result = await request();
      // Empty pulls also prove recovery. One direction cannot clear the other.
      if (generation === replicationGeneration) {
        state(direction, null);
        state("replication", null);
      }
      return result;
    } catch (error) {
      if (generation === replicationGeneration) {
        state(direction, error);
      }
      throw error;
    }
  };
  const finishPullSequenceWhenIdle = () => {
    const activeDown = replication.internalReplicationState?.events.active.down;
    const finish = () => {
      pullActivitySubscription?.unsubscribe();
      pullActivitySubscription = undefined;
      pullInFlight = false;
      servePendingPull();
    };
    if (!activeDown || !activeDown.getValue()) {
      finish();
      return;
    }
    pullActivitySubscription?.unsubscribe();
    pullActivitySubscription = activeDown.subscribe((active) => {
      if (!active) finish();
    });
  };
  const replicationIdentifier = `${workspaceId}:drafts:v1`;
  const createReplication = () => {
    const generation = advanceReplicationGeneration();
    const current = replicateRxCollection<SyncDocument, { seq: number }>({
      collection: db.drafts,
      replicationIdentifier,
      live: true,
      retryTime: DRAFT_PUSH_RETRY_TIME_MS,
      pull: {
        handler: async (checkpoint, batchSize) => {
          if (!beginTriggeredPull())
            return { documents: [], checkpoint: checkpoint || { seq: 0 } };
          try {
            return await attempt(generation, "pull", async () => {
              const result = await pull(
                "drafts",
                checkpoint?.seq || 0,
                batchSize,
                workspaceId,
                verifyWorkspace,
              );
              if (result.reset === true)
                throw new Error("The server reset draft sync unexpectedly.");
              pullContinues = result.documents.length >= batchSize;
              return {
                documents: result.documents,
                checkpoint: result.checkpoint,
              };
            });
          } finally {
            if (failures.has("pull")) pullContinues = false;
            if (!pullContinues) finishPullSequenceWhenIdle();
          }
        },
        batchSize: 100,
      },
      push: {
        waitBeforePersist,
        handler: (rows) =>
          attempt(generation, "push", async () => {
            await verifyWorkspace();
            return syncPost("/api/sync/drafts", { rows }, { workspaceId });
          }),
        batchSize: 100,
      },
    });
    if (options.testOnly?.onCreate) {
      try {
        options.testOnly.onCreate();
      } catch {
        // Explicit test instrumentation must not affect the lifecycle.
      }
    }
    replicationErrors = [
      current.error$.subscribe((error: any) => {
        if (generation !== replicationGeneration) return;
        const direction =
          error.code === "RC_PULL"
            ? "pull"
            : error.code === "RC_PUSH"
              ? "push"
              : "replication";
        state(direction, error);
      }),
    ];
    replication = current;
    return current;
  };
  createReplication();
  const waitForDownstreamInitialSync = async (current: typeof replication) => {
    await raceStopWithCap(current.startPromise);
    if (stopped) return;
    const downSync = current.internalReplicationState?.firstSyncDone.down;
    if (!downSync || downSync.getValue()) return;
    let subscription: { unsubscribe: () => void } | undefined;
    const downstream = new Promise<void>((resolve) => {
      subscription = downSync.subscribe((done) => {
        if (done) resolve();
      });
    });
    await raceStopWithCap(downstream, () => subscription?.unsubscribe());
  };
  const scheduleRestart = () => {
    if (
      stopped ||
      !pushFailed ||
      !pullTriggered ||
      pullInFlight ||
      pullContinues ||
      restarting ||
      restartTimer
    )
      return;
    const dueAt = lastRestartAt + DRAFT_PUSH_RESTART_MIN_INTERVAL_MS;
    const delay = Math.max(0, dueAt - performance.now());
    restartTimer = setTimeout(() => {
      restartTimer = undefined;
      if (stopped || !pullTriggered || pullInFlight || pullContinues) return;
      if (!pushFailed) {
        replication.reSync();
        return;
      }
      const previous = replication;
      advanceReplicationGeneration();
      replicationErrors.forEach((subscription) => subscription.unsubscribe());
      replicationErrors = [];
      settlePushWait();
      restarting = (async () => {
        try {
          await cancelReplication(previous);
        } catch (error) {
          state("replication", error);
        }
        if (stopped) return;
        lastRestartAt = performance.now();
        // RxDB starts both directions in parallel. This fresh state's initial
        // pull can start while its upstream retries a failed push.
        const current = createReplication();
        await waitForDownstreamInitialSync(current);
      })()
        .catch((error) => {
          state("replication", error);
        })
        .finally(() => {
          restarting = undefined;
          servePendingPull();
        });
    }, delay);
  };
  const servePendingPull = () => {
    if (
      stopped ||
      !pullTriggered ||
      pullInFlight ||
      pullContinues ||
      restarting
    )
      return;
    if (pushFailed) scheduleRestart();
    else replication.reSync();
  };
  const triggerPull = () => {
    pullTriggered = true;
    servePendingPull();
  };
  const stopInvalidation = watchSyncInvalidations(() => {
    triggerPull();
  }, "drafts");
  const stopResume = onResume(() => {
    triggerPull();
  });
  return async () => {
    stopped = true;
    clearTimeout(restartTimer);
    restartTimer = undefined;
    for (const wake of stopWaiters) wake();
    pullActivitySubscription?.unsubscribe();
    pullActivitySubscription = undefined;
    stopInvalidation();
    stopResume();
    replicationErrors.forEach((subscription) => subscription.unsubscribe());
    replicationErrors = [];
    // Settle the gate first: cancellation waits for replication work that may
    // currently be paused in waitBeforePersist().
    settlePushWait();
    try {
      await restarting;
      await cancelReplication(replication);
    } catch (error) {
      report(error, "replication");
    } finally {
      settlePushWait();
    }
  };
}

// Reopen a subscription after an initial connection failure, without a page reload.
function subscribeProjection<T>(
  watch: (
    accept: (payload: T) => void,
    fail: (error: unknown) => void,
  ) => Promise<() => void>,
  accept: (payload: T) => void,
  report: (error: unknown | null) => void,
  retryBeforeData?: () => Promise<unknown>,
) {
  let stopped = false,
    connecting = false,
    attached = false;
  let dispose = () => {};
  let hasData = false;
  let retryCount = 0;
  let retryTimer: ReturnType<typeof setTimeout> | undefined;
  const clearRetry = () => {
    clearTimeout(retryTimer);
    retryTimer = undefined;
  };
  const onFailure = (error: unknown | null) => {
    if (stopped) return;
    report(error);
    if (error === null) {
      retryCount = 0;
      clearRetry();
      return;
    }
    if (
      !retryBeforeData ||
      hasData ||
      retryTimer !== undefined ||
      !retryableReadError(error) ||
      document.hidden ||
      navigator.onLine === false
    )
      return;
    retryTimer = setTimeout(() => {
      retryTimer = undefined;
      if (stopped || hasData) return;
      if (attached) void retryBeforeData().catch(onFailure);
      else void connect();
    }, readRetryDelay(retryCount++));
  };
  const connect = async () => {
    if (stopped || connecting || attached) return;
    clearRetry();
    connecting = true;
    try {
      const stop = await watch((value) => {
        if (!stopped) {
          if (value !== null) {
            hasData = true;
            clearRetry();
            retryCount = 0;
            onFailure(null);
          }
          accept(value);
        }
      }, onFailure);
      if (stopped) stop();
      else {
        dispose = stop;
        attached = true;
      }
    } catch (error) {
      if (!stopped) {
        onFailure(error);
      }
    } finally {
      connecting = false;
    }
  };
  const stopResume = onResume(() => void connect());
  const stopConnection = watchResourceConnection((status) => {
    if (status === "live" && !attached) void connect();
  });
  void connect();
  return () => {
    stopped = true;
    stopResume();
    stopConnection();
    dispose();
    clearRetry();
  };
}

export function subscribeStateProjection(
  accept: (payload: StateProjectionPayload) => void,
  report: (error: unknown | null) => void,
) {
  return subscribeProjection(watchStateProjection, accept, report, () =>
    refreshProjection(),
  );
}

export function subscribeTranscriptProjection(
  scope: `transcript:${string}`,
  accept: (payload: TranscriptProjectionPayload) => void,
  report: (error: unknown | null) => void,
) {
  return subscribeProjection(
    (next, fail) => watchTranscriptProjection(scope, next, fail),
    accept,
    report,
  );
}
