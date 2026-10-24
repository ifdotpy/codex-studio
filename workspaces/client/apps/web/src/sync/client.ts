import { SUBSCRIPTION_ENTITY_SCOPES } from "../generated/subscriptionEntities";
import { cursorDocumentId, parseAppliedCursor } from "./subscriptions/cursors";
import { serverDatabaseSuffix } from "../servers/environment";

import {
  MAX_SAFE_INTEGER,
  SYNC_ENTITY_STREAM_MAX_DOCUMENTS,
  SYNC_TRANSCRIPT_STREAM_MAX_ITEMS,
} from "../generated/sharedValues";

import { createRxDatabase, addRxPlugin } from "rxdb";

import { getRxStorageDexie } from "rxdb/plugins/storage-dexie";

import { RxDBLeaderElectionPlugin } from "rxdb/plugins/leader-election";

import { replicateRxCollection } from "rxdb/plugins/replication";

import type { RxCollection, RxDocumentData } from "rxdb";

import { createDraftConflictHandler } from "./conflicts";

import {
  applyEntityRows,
  emptyEntityProjection,
  type UiPreferenceProjection,
  type EntityRow,
} from "./entityProjection";

import { observeEntityProjection } from "./entityProjectionObserver";

import {
  syncGet,
  syncPost,
  ApiError,
  saved,
  save,
  setWorkspace,
  isApiSchemaMismatch,
  onMatchingApiSchemaResponse,
} from "../api";

import type { SyncEntityPayload } from "../generated/api";

import type { Snapshot, TranscriptPageData } from "../types";

import {
  entitySequenceInvalidationCovered,
  EntitySequenceCheckpoint,
  getEntitySequenceCheckpoint,
  setEntitySequenceProjection,
} from "./entitySequence";

import {
  watchResourceChanges,
  watchResourceConnection,
  type ResourceConnectionState,
  type ResourceRef,
  type ResourceVersion,
} from "./subscriptions/events";

import { onResume } from "./resume";

import {
  ReadRecoveryBudget,
  ReadRecoveryExhaustedError,
  retryableReadError,
  retryableTranscriptPullConflict,
} from "./readRetry";

import { refreshAfterCurrentPull } from "./refreshAfterCurrentPull";

import { isEntityResetResponse, requiredSyncNumber } from "./pullContract";

import {
  cacheTranscriptValue,
  peekTranscript,
  patchTranscriptValue,
  subscribeTranscript,
} from "./transcriptCache";

import { DRAFT_SYNC_TIMING_MS } from "./draftSyncTiming.mjs";

import { API_SCHEMA_HASH } from "../generated/apiSchema";

import { SYNC_ENTITY_SCOPE, type components } from "../generated/api";

import {
  canSerializeEntityProjectionWrites,
  withEntityProjectionWrite,
} from "./entityProjectionWrites";

import {
  ENTITY_PROJECTION_CACHE_VERSION,
  deleteOtherEntityProjectionDatabases,
  entityProjectionDatabaseName,
  workspaceDatabaseName,
  holdEntityProjectionDatabaseLock,
} from "./entityCacheStorage";

export { DRAFT_SYNC_TIMING_MS } from "./draftSyncTiming.mjs";

addRxPlugin(RxDBLeaderElectionPlugin);

export type SyncDocument = EntityRow;

/** Derive the local primary key once; entities remain structured in IndexedDB. */
export function entityCacheRow(
  change: components["schemas"]["SyncEntity"],
): EntityRow {
  const { op, seq, ...entity } = change;
  return {
    id: `entity:${change.collection}:${change.id}`,
    seq,
    payload: "",
    _deleted: op === "delete",
    ...(op === "upsert" ? { entity: entity as SyncEntityPayload } : {}),
  };
}

export {
  watchResourceChanges,
  watchResourceConnection,
  type ResourceConnectionState,
} from "./subscriptions/events";

export type { ResourceRef } from "./subscriptions/events";

type TranscriptSyncPayload = components["schemas"]["TranscriptSyncPayload"];

type TranscriptProjectionMeta = {
  format: 2;
  epoch?: string;
  metadata: Omit<Partial<TranscriptPageData>, "items">;
  order: string[];
  itemRevisions: Record<string, string>;
};

type ProjectionState = {
  users: number;
  foreground: number;
  stop: () => Promise<unknown>;
  refresh: (signal?: AbortSignal) => Promise<void>;
  refreshAfterCurrent: () => Promise<void>;
  resetReadRecovery: () => void;
  activateInvalidation: () => void;
  listeners: Set<(error: unknown | null) => void>;
};

type StateProjectionPayload = Snapshot | null;

type TranscriptProjectionPayload = TranscriptPageData | null;

import { clearTranscript } from "./transcriptCache";
export type SyncClientOptions = {
  api: Pick<
    import("../api").ServerApi,
    | "syncGet"
    | "syncPost"
    | "saved"
    | "save"
    | "setWorkspace"
    | "isApiSchemaMismatch"
    | "onMatchingApiSchemaResponse"
  >;
  resources: Pick<
    typeof import("./subscriptions/events"),
    "watchResourceChanges" | "watchResourceConnection"
  >;
  sequences: Pick<
    ReturnType<typeof import("./entitySequence").createEntitySequenceScope>,
    "getEntitySequenceCheckpoint" | "setEntitySequenceProjection"
  >;
  transcripts: ReturnType<
    typeof import("./transcriptCache").createTranscriptCache
  >;
  databaseSuffix: string;
};
export function createSyncClient(options: SyncClientOptions) {
  const {
    syncGet,
    syncPost,
    saved,
    save,
    setWorkspace,
    isApiSchemaMismatch,
    onMatchingApiSchemaResponse,
  } = options.api;
  const { watchResourceChanges, watchResourceConnection } = options.resources;
  const { getEntitySequenceCheckpoint, setEntitySequenceProjection } =
    options.sequences;
  const {
    clearTranscript,
    cacheTranscriptValue,
    peekTranscript,
    patchTranscriptValue,
    subscribeTranscript,
  } = options.transcripts;
  const serverDatabaseSuffix = options.databaseSuffix;

  const DRAFT_PUSH_QUIET_WAIT_MS = DRAFT_SYNC_TIMING_MS.pushQuietWait;

  const DRAFT_PUSH_MAX_WAIT_MS = DRAFT_SYNC_TIMING_MS.pushMaxWait;

  const DRAFT_PUSH_RETRY_TIME_MS = DRAFT_SYNC_TIMING_MS.pushRetry;

  // Failed pushes already retry this often; restart triggers must not accelerate them.
  const DRAFT_PUSH_RESTART_MIN_INTERVAL_MS =
    DRAFT_SYNC_TIMING_MS.restartMinInterval;

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
        maximum: MAX_SAFE_INTEGER,
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
    let cleanupScheduled = false;
    let projectionDatabaseLockHeld = false;
    let stopSchemaMatchCleanup = () => {};
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
      const unavailable = isApiSchemaMismatch() || retryableReadError(error);
      if (!hasCache || !unavailable) throw error;
    } finally {
      clearTimeout(grace);
    }
    const workspaceId = identity?.workspaceId || cached;
    setEntitySequenceProjection(workspaceId, API_SCHEMA_HASH);
    if (identity) save("codex-sync-workspace", workspaceId);
    const cleanupOtherSchemaCaches = () => {
      if (
        cleanupScheduled ||
        !projectionDatabaseLockHeld ||
        isApiSchemaMismatch()
      )
        return;
      cleanupScheduled = true;
      stopSchemaMatchCleanup();
      deleteOtherEntityProjectionDatabases(
        workspaceId,
        API_SCHEMA_HASH,
        serverDatabaseSuffix,
      );
    };
    // Cached display does not authorize reads or draft writes against another computer.
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
      name: workspaceDatabaseName(workspaceId, serverDatabaseSuffix),
      storage: getRxStorageDexie(),
      multiInstance: true,
    });
    const draftConflicts = createDraftConflictHandler();
    await db.addCollections({
      drafts: { schema, conflictHandler: draftConflicts.conflictHandler },
      outbox: { schema },
    });
    const currentProjectionDatabase = entityProjectionDatabaseName(
      workspaceId,
      API_SCHEMA_HASH,
      serverDatabaseSuffix,
    );
    projectionDatabaseLockHeld = await holdEntityProjectionDatabaseLock(
      currentProjectionDatabase,
    );
    const projectionDb = await createRxDatabase({
      name: currentProjectionDatabase,
      storage: getRxStorageDexie(),
      multiInstance: true,
    });
    const { projections } = await projectionDb.addCollections({
      projections: {
        // The schema-hashed database starts empty after a wire change.
        autoMigrate: false,
        schema: {
          ...schema,
          version: ENTITY_PROJECTION_CACHE_VERSION,
          properties: {
            ...schema.properties,
            entity: { type: "object", additionalProperties: true },
          },
        },
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
    });
    Object.defineProperty(db, "projections", { value: projections });
    if (isApiSchemaMismatch()) stopSchemaMatchCleanup();
    else {
      stopSchemaMatchCleanup = onMatchingApiSchemaResponse(
        cleanupOtherSchemaCaches,
      );
      if (cleanupScheduled) stopSchemaMatchCleanup();
    }
    db.projections.$.subscribe((event) => {
      const doc = event.documentData;
      if (doc.id.startsWith("transcript:") && doc.id.split(":").length === 2)
        cacheStoredTranscriptAfterWrite(
          db.projections,
          workspaceId,
          doc.id,
          doc,
        );
    });
    return { db, workspaceId, verifyWorkspace, draftConflicts };
  }

  function syncDatabase() {
    return (pending ??= open().catch((error) => {
      pending = undefined;
      throw error;
    }));
  }

  async function persistEntityDocuments(
    targetWorkspaceId: string,
    documents: components["schemas"]["SyncEntity"][],
    syncEntitiesAfter: number | null | undefined,
    isPersistenceCurrent: () => boolean,
    streamEpoch?: string,
  ) {
    const { db } = await syncDatabase();
    try {
      await withEntityProjectionWrite(
        db.projections.database.name,
        async () => {
          const checkpoint = getEntitySequenceCheckpoint(
            SYNC_ENTITY_SCOPE,
            API_SCHEMA_HASH,
          );
          if (streamEpoch) {
            if (!isPersistenceCurrent()) return;
            checkpoint.observeEpoch(streamEpoch);
            const [durable, ready] =
              await db.projections.storageInstance.findDocumentsById(
                ["state:entities:checkpoint", "state:entities:ready"],
                true,
              );
            if (
              !isPersistenceCurrent() ||
              !checkpoint.canUseDurableCheckpoint ||
              ready?.payload !== "ready" ||
              typeof syncEntitiesAfter !== "number"
            )
              return;
            checkpoint.assignWithinEpoch(durable?.seq ?? 0);
            // Covered intervals can contain rows deleted by a later reset. Never
            // insert their old data over the reset's zero-sequence tombstones.
            const through = Math.max(
              ...documents.map((document) => document.seq),
            );
            if (
              through <= checkpoint.value ||
              syncEntitiesAfter > checkpoint.value ||
              syncEntitiesAfter > (durable?.seq ?? 0)
            )
              return;
          }
          const resetVersion = checkpoint.resetVersion;
          await persistEntityDocumentsUnlocked(
            targetWorkspaceId,
            documents,
            syncEntitiesAfter,
            () =>
              isPersistenceCurrent() &&
              checkpoint.resetVersion === resetVersion,
          );
        },
      );
    } catch (error) {
      await refreshProjection().catch(() => {});
      throw error;
    }
  }

  async function persistEntityDocumentsUnlocked(
    targetWorkspaceId: string,
    documents: components["schemas"]["SyncEntity"][],
    syncEntitiesAfter: number | null | undefined,
    isPersistenceCurrent: () => boolean,
  ) {
    const { db, workspaceId } = await syncDatabase();
    if (
      !isPersistenceCurrent() ||
      (targetWorkspaceId && targetWorkspaceId !== workspaceId)
    )
      return;
    const checkpoint = getEntitySequenceCheckpoint(
      SYNC_ENTITY_SCOPE,
      API_SCHEMA_HASH,
    );
    const resetVersion = checkpoint.resetVersion;
    const entities = documents.map(entityCacheRow);
    let checkpointBefore = 0;
    await persistProjectionBatch(
      db.projections,
      entities,
      isPersistenceCurrent,
    );
    if (isPersistenceCurrent() && entities.length > 0) {
      const [persistedCheckpoint, readyMarker] =
        await db.projections.storageInstance.findDocumentsById(
          ["state:entities:checkpoint", "state:entities:ready"],
          true,
        );
      checkpointBefore = persistedCheckpoint?.seq ?? 0;
      if (
        readyMarker?.payload !== "resetting" &&
        checkpoint.canUseDurableCheckpoint &&
        checkpoint.resetVersion === resetVersion &&
        checkpointBefore > checkpoint.value
      ) {
        // Another tab can durably advance the shared projection row while
        // this tab's in-memory checkpoint remains behind. Reconcile that
        // durable shadow before evaluating this response's contiguous
        // checkpoint, while retaining the projection reset fence.
        checkpoint.assignWithinEpoch(checkpointBefore);
      }
      const highestSequence = Math.max(
        ...entities.map((document) => document.seq),
      );
      if (
        typeof syncEntitiesAfter === "number" &&
        isPersistenceCurrent() &&
        readyMarker?.payload !== "resetting"
      ) {
        const checkpointAdvanced = await checkpoint.advanceIfContiguous(
          syncEntitiesAfter,
          highestSequence,
          resetVersion,
          checkpointBefore,
          async () => {
            if (!isPersistenceCurrent())
              throw new Error("Stream persistence is no longer current");
            await persistProjection(db.projections, {
              id: "state:entities:checkpoint",
              payload: persistedCheckpoint?.payload ?? "{}",
              seq: highestSequence,
            });
          },
        );
        if (checkpointAdvanced) {
          checkpoint.markDurableCheckpointValid();
        }
      }
    }
  }

  async function readSubscriptionCursor(resource: ResourceRef) {
    const { db, workspaceId } = await syncDatabase();
    const [row] = await db.projections.storageInstance.findDocumentsById(
      [await cursorDocumentId(resource)],
      false,
    );
    const cursor = parseAppliedCursor(row?.payload, workspaceId);
    if (cursor && resource.kind === "transcript") {
      const [base] = await db.projections.storageInstance.findDocumentsById(
        [`transcript:${resource.agentId}`],
        false,
      );
      if (!base || base.seq < cursor.seq || !readTranscriptMeta(base.payload))
        return undefined;
    }
    return cursor;
  }

  async function persistSubscriptionCursor(
    resource: ResourceRef,
    cursor: components["schemas"]["SubscriptionCursor"],
    isCurrent: () => boolean,
  ) {
    const { db, workspaceId } = await syncDatabase();
    if (!isCurrent() || workspaceId !== cursor.workspaceId) return;
    await persistProjection(db.projections, {
      id: await cursorDocumentId(resource),
      payload: JSON.stringify(cursor),
      seq: cursor.seq,
    });
  }

  async function persistScopedEntities(
    resource: ResourceRef,
    entry: components["schemas"]["ResourceRevisionEntry"],
    isCurrent: () => boolean,
  ) {
    const cursor = entry.cursor;
    const changes = entry.entityChanges;
    if (!cursor || !changes) return false;
    const scopes: Record<
      string,
      {
        collections: readonly string[];
        idField: string | null;
        agentField: string | null;
      }
    > = SUBSCRIPTION_ENTITY_SCOPES;
    const scope = scopes[resource.kind];
    if (!scope) return false;
    const { db, workspaceId } = await syncDatabase();
    if (
      cursor.workspaceId !== workspaceId ||
      cursor.seq !== entry.revision ||
      changes.through !== cursor.seq
    )
      throw new Error("Invalid subscription cursor interval");
    const selector = resource as unknown as Record<string, unknown>;
    const belongs = (entity: {
      collection: string;
      id: string;
      value?: unknown;
    }) => {
      if (!scope.collections.includes(entity.collection)) return false;
      if (scope.idField && entity.id !== selector[scope.idField]) return false;
      if (
        scope.agentField &&
        selector[scope.agentField] &&
        entity.value &&
        (entity.value as { agent?: string }).agent !==
          selector[scope.agentField] &&
        (entity.value as { rootId?: string }).rootId !==
          selector[scope.agentField]
      )
        return false;
      return true;
    };
    await withEntityProjectionWrite(db.projections.database.name, async () => {
      if (!isCurrent()) return;
      const applied = await readSubscriptionCursor(resource);
      if (applied && applied.seq > cursor.seq) return;
      if (entry.replyKind !== "snapshot" && applied?.seq !== changes.after)
        throw new Error("Subscription delta has no applied local base");
      if (
        changes.documents.some(
          (row) =>
            !belongs(row) ||
            row.seq > cursor.seq ||
            (entry.replyKind !== "snapshot" && row.seq <= changes.after),
        )
      )
        throw new Error("Entity outside subscription interval");
      const rows = changes.documents.map(entityCacheRow);
      if (entry.replyKind === "snapshot") {
        const incoming = new Set(rows.map((row) => row.id));
        for (const collection of scope.collections) {
          const prefix = `entity:${collection}:`;
          const existing = await db.projections
            .find({
              selector: { id: { $gte: prefix, $lt: `entity:${collection};` } },
            })
            .exec();
          for (const doc of existing) {
            const row = doc.toJSON();
            if (row.entity && belongs(row.entity) && !incoming.has(row.id))
              rows.push({
                id: row.id,
                payload: "",
                seq: cursor.seq,
                _deleted: true,
              });
          }
        }
      }
      await persistProjectionBatch(db.projections, rows, isCurrent);
      if (!isCurrent()) return;
      if (resource.kind === "state") {
        await persistProjection(db.projections, {
          id: "state:entities:checkpoint",
          payload: "{}",
          seq: cursor.seq,
        });
        await persistProjection(db.projections, {
          id: "state:entities:ready",
          payload: "ready",
          seq: cursor.seq,
        });
      }
      await persistSubscriptionCursor(resource, cursor, isCurrent);
    });
    return true;
  }

  async function persistResourceEntityChanges(
    event: import("./subscriptions/events").ResourceChangeEvent,
    isCurrent: () => boolean,
  ) {
    // Older browsers keep the existing pull path. Tab-local serialization cannot
    // protect a shared projection from another tab's reset.
    if (!canSerializeEntityProjectionWrites()) {
      if (event.resourceVersions.some((entry) => entry.cursor))
        throw new Error(
          "Subscription cursor persistence requires the projection write lock",
        );
      return;
    }
    const { db, workspaceId } = await syncDatabase();
    if (!isCurrent() || workspaceId !== event.workspaceId) return;
    for (const [index, entry] of event.resourceVersions.entries()) {
      if (!isCurrent()) return;
      const resource = entry.resource ?? event.resources[index];
      if (
        resource &&
        entry.cursor &&
        entry.entityChanges &&
        (await persistScopedEntities(resource, entry, isCurrent))
      )
        continue;
      if (
        resource &&
        entry.cursor &&
        entry.replyKind === "delta" &&
        !entry.transcriptChanges &&
        !entry.transcriptSnapshot &&
        !entry.entityChanges
      ) {
        const applied = await readSubscriptionCursor(resource);
        if (!applied || applied.seq < entry.cursor.seq)
          throw new Error("Empty subscription delta has no applied local base");
        continue;
      }

      if (resource?.kind === "transcript" && entry.transcriptSnapshot) {
        if (
          (event.reason !== "initial" && event.reason !== "reconnect") ||
          entry.transcriptSnapshot.items.length >
            SYNC_TRANSCRIPT_STREAM_MAX_ITEMS
        )
          throw new TypeError("Invalid stream transcript snapshot");
        await persistTranscriptProjection(
          db.projections,
          `transcript:${resource.agentId}`,
          {
            id: `transcript:${resource.agentId}`,
            payload: "",
            seq: entry.revision,
            _deleted: false,
          },
          workspaceId,
          entry.transcriptSnapshot,
          undefined,
          isCurrent,
          event.epoch,
          entry.cursor?.workspaceId === workspaceId,
        );
        if (entry.cursor && isCurrent()) {
          const [stored] =
            await db.projections.storageInstance.findDocumentsById(
              [`transcript:${resource.agentId}`],
              false,
            );
          if (stored?.seq === entry.cursor.seq)
            await persistSubscriptionCursor(resource, entry.cursor, isCurrent);
        }
        continue;
      }
      if (resource?.kind === "transcript" && entry.transcriptChanges) {
        const batch = entry.transcriptChanges;
        if (
          batch.through !== entry.revision ||
          batch.after < 0 ||
          batch.through <= batch.after ||
          batch.delta.items.length + batch.delta.removed.length >
            SYNC_TRANSCRIPT_STREAM_MAX_ITEMS
        )
          throw new TypeError("Invalid stream transcript batch");
        await persistTranscriptProjection(
          db.projections,
          `transcript:${resource.agentId}`,
          {
            id: `transcript:${resource.agentId}`,
            payload: "",
            seq: batch.through,
            _deleted: false,
          },
          workspaceId,
          batch.delta,
          batch.after,
          isCurrent,
          event.epoch,
          entry.cursor?.workspaceId === workspaceId,
        );
        if (entry.cursor && isCurrent()) {
          const [stored] =
            await db.projections.storageInstance.findDocumentsById(
              [`transcript:${resource.agentId}`],
              false,
            );
          if (stored?.seq === entry.cursor.seq)
            await persistSubscriptionCursor(resource, entry.cursor, isCurrent);
        }
        continue;
      }
      const changes = entry.entityChanges;
      if (resource?.kind !== "state" || !changes || entry.entitySequenceReset)
        continue;
      if (
        !Number.isSafeInteger(changes.after) ||
        changes.after < 0 ||
        !Number.isSafeInteger(changes.through) ||
        changes.through <= changes.after ||
        changes.through !== entry.revision ||
        !Array.isArray(changes.documents) ||
        !changes.documents.length ||
        changes.documents.length > SYNC_ENTITY_STREAM_MAX_DOCUMENTS ||
        changes.documents.some(
          (row) =>
            typeof row.id !== "string" ||
            typeof row.collection !== "string" ||
            (row.op !== "upsert" && row.op !== "delete") ||
            (row.op === "upsert" &&
              (!row.value ||
                typeof row.value !== "object" ||
                Array.isArray(row.value))) ||
            !Number.isSafeInteger(row.seq) ||
            row.seq <= changes.after ||
            row.seq > changes.through,
        ) ||
        Math.max(...changes.documents.map((row) => row.seq)) !== changes.through
      )
        throw new TypeError("Invalid stream entity batch");
      await persistEntityDocuments(
        workspaceId,
        changes.documents,
        changes.after,
        isCurrent,
        event.epoch,
      );
    }
  }

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
        ...(scope === SYNC_ENTITY_SCOPE ? { reset: "1" } : {}),
        ...(priorityId ? { priorityId } : {}),
      },
      ...(signal ? { signal } : {}),
    });
    if (result.workspaceId !== workspaceId) throw new WorkspaceMismatchError();
    return result;
  }

  function resourceForScope(scope: string): ResourceRef {
    if (scope === "drafts") return { kind: "drafts" };
    if (scope.startsWith("transcript:"))
      return { kind: "transcript", agentId: scope.slice("transcript:".length) };
    return { kind: "state" };
  }

  function watchSyncInvalidations(
    resync: (version?: ResourceVersion) => void,
    scope?: string,
  ): () => void;

  function watchSyncInvalidations(
    scope: string,
    resync: (version?: ResourceVersion) => void,
  ): () => void;

  function watchSyncInvalidations(
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
  async function persistProjection(
    collection: RxCollection<SyncDocument>,
    incoming: SyncDocument,
    replaceObsoleteTranscript = false,
  ) {
    for (;;) {
      const [previous] = await collection.storageInstance.findDocumentsById(
        [incoming.id],
        true,
      );
      if (
        previous &&
        previous.seq >= incoming.seq &&
        !replaceObsoleteTranscript
      )
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
    isCurrent: () => boolean = () => true,
  ) {
    const pending = new Map(documents.map((row) => [row.id, row]));
    while (pending.size) {
      if (!isCurrent()) return;
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
      if (!isCurrent()) return;
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
    return withEntityProjectionWrite(collection.database.name, () =>
      resetEntityProjectionUnlocked(collection),
    );
  }

  async function resetEntityProjectionUnlocked(
    collection: RxCollection<SyncDocument>,
  ) {
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
      ...(initial
        ? [raw("state:entities:initial", "{}", 0, true, initial)]
        : []),
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

  const transcriptItemId = (scope: string, id: string) =>
    `transcript:item:${scope.slice("transcript:".length)}:${id}`;

  function isTranscriptDelta(
    value: TranscriptSyncPayload,
  ): value is components["schemas"]["TranscriptStreamDelta"] {
    return "delta" in value;
  }

  function transcriptMetadata(value: TranscriptSyncPayload) {
    if (isTranscriptDelta(value)) {
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
    const { items: _items, ...metadata } = value;
    return metadata;
  }

  function readTranscriptMeta(
    payload: string,
  ): TranscriptProjectionMeta | null {
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

  /** A store subscriber can see a meta row before its writer updates the
   * cache. Check after that write settles, so a patched cache skips the read. */
  function cacheStoredTranscriptAfterWrite(
    collection: RxCollection<SyncDocument>,
    workspaceId: string,
    scope: string,
    doc: SyncDocument & { _deleted?: boolean },
  ) {
    setTimeout(
      () => void cacheStoredTranscript(collection, workspaceId, scope, doc),
      0,
    );
  }

  async function persistTranscriptProjection(
    collection: RxCollection<SyncDocument>,
    scope: string,
    document: SyncDocument & { _deleted?: boolean },
    workspaceId: string,
    incoming: TranscriptSyncPayload | null,
    after?: number,
    isCurrent: () => boolean = () => true,
    epoch?: string,
    durableResume = false,
  ) {
    return withEntityProjectionWrite(collection.database.name, async () => {
      if (!isCurrent()) return;
      const [previous] = await collection.storageInstance.findDocumentsById(
        [scope],
        true,
      );
      const base = previous?.payload
        ? readTranscriptMeta(previous.payload)
        : null;
      const epochChanged =
        !durableResume &&
        base?.epoch !== undefined &&
        epoch !== undefined &&
        base.epoch !== epoch;
      if (epochChanged && incoming !== null && isTranscriptDelta(incoming))
        throw new Error("Transcript delta belongs to a different epoch.");
      if (!epochChanged && previous && previous.seq >= document.seq && base)
        return;
      if (after !== undefined && previous?.seq !== after)
        throw new Error("Transcript stream interval has no local base.");
      const id = scope.slice("transcript:".length);
      const oldOrder: string[] = base?.order || [];
      const delta =
        incoming !== null && isTranscriptDelta(incoming) ? incoming : null;
      const isDelta = delta !== null;
      if (isDelta && !base)
        throw new Error("Transcript delta has no local item base.");

      let changedItems = incoming?.items ?? [];
      // A window baseline omits older rows present in the shared pull projection.
      // Keep only a contiguous retained suffix when that projection sends its
      // complete order; an edit outside the window must not bridge a missing row.
      let windowOrder: string[] | undefined;
      let windowTruncated = false;
      if (delta && base?.metadata.truncated) {
        const available = new Set(
          oldOrder.filter((key) => !delta.removed.includes(key)),
        );
        if (delta.order) {
          for (const item of changedItems) available.add(item.id);
          let start = delta.order.length;
          while (start > 0 && available.has(delta.order[start - 1])) start--;
          windowOrder = delta.order.slice(start);
          windowTruncated = start > 0;
        } else {
          windowOrder = oldOrder.filter((key) => available.has(key));
          windowTruncated = true;
        }
        const retained = new Set(windowOrder);
        changedItems = changedItems.filter((item) => retained.has(item.id));
      }
      const removed =
        incoming === null ? [...oldOrder] : delta ? [...delta.removed] : [];
      const order =
        incoming === null
          ? []
          : windowOrder
            ? windowOrder
            : delta?.order
              ? delta.order
              : isDelta
                ? oldOrder
                    .filter((key) => !removed.includes(key))
                    .concat(
                      changedItems
                        .map((item) => item.id)
                        .filter((key: string) => !oldOrder.includes(key)),
                    )
                : changedItems.map((item) => item.id);
      if (!document._deleted && !isDelta)
        removed.push(...oldOrder.filter((key) => !order.includes(key)));
      const previousMetadata = isDelta ? base?.metadata || {} : {};
      const metadata: TranscriptProjectionMeta["metadata"] =
        incoming === null
          ? {}
          : { ...previousMetadata, ...transcriptMetadata(incoming) };
      if (windowTruncated) {
        metadata.truncated = true;
        metadata.nextCursor = order[0] ?? base?.metadata.nextCursor ?? null;
      }
      const itemRevisions =
        incoming === null
          ? {}
          : { ...base?.itemRevisions, ...delta?.itemRevisions };
      for (const itemId of removed) delete itemRevisions[itemId];
      const nextMeta: TranscriptProjectionMeta = {
        format: 2,
        epoch: epoch ?? base?.epoch,
        metadata,
        order,
        itemRevisions,
      };
      const rows: SyncDocument[] = changedItems.map((item) => ({
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
      if (epochChanged) {
        clearTranscript(workspaceId, id);
        const existing = await collection.storageInstance.findDocumentsById(
          rows.map((row) => row.id),
          true,
        );
        const byId = new Map(existing.map((row) => [row.id, row]));
        if (!isCurrent()) return;
        await writeProjectionRows(
          collection,
          rows.map((row) => ({
            previous: byId.get(row.id),
            document: {
              ...row,
              _deleted: row._deleted === true,
              _attachments: {},
              _meta: { lwt: 1 },
              _rev: "",
            },
          })),
        );
      } else if (rows.length)
        await persistProjectionBatch(collection, rows, isCurrent);
      if (!isCurrent()) return;
      const metaDocument: SyncDocument = {
        id: scope,
        payload: JSON.stringify(nextMeta),
        seq: document.seq,
        _deleted: document._deleted,
      };
      await persistProjection(
        collection,
        metaDocument,
        epochChanged || (!document._deleted && !isDelta && !!previous && !base),
      );
      if (!isCurrent()) return;
      if (incoming !== null && !isDelta) {
        cacheTranscriptValue(
          workspaceId,
          id,
          { ...metadata, items: changedItems },
          document.seq,
          document.payload.length * 2,
        );
      } else if (incoming !== null) {
        const cacheMetadata = metadata;
        // Patch only a cache entry at this delta's durable base. A write that
        // stopped after its durable update can leave the cache behind it.
        const patched =
          peekTranscript(workspaceId, id)?.seq === previous?.seq &&
          patchTranscriptValue(
            workspaceId,
            id,
            cacheMetadata,
            document.seq,
            changedItems,
            removed,
            delta?.order || windowOrder ? order : undefined,
          );
        if (!patched)
          await cacheStoredTranscript(
            collection,
            workspaceId,
            scope,
            metaDocument,
          );
      } else {
        cacheTranscriptValue(workspaceId, id, null, document.seq, 0);
      }
    });
  }

  const scopes = new Map<string, ProjectionState>();

  const closingScopes = new Map<string, Promise<unknown>>();

  const ENTITY_BATCH_SIZE = 500;

  const isTransientSyncReadFailure = retryableReadError;

  async function acquireProjection(
    scope: string,
    expectedWorkspace?: string,
    background = false,
  ) {
    const { db, workspaceId, verifyWorkspace } = await syncDatabase();
    if (expectedWorkspace && expectedWorkspace !== workspaceId)
      throw new WorkspaceMismatchError();
    while (closingScopes.has(scope)) await closingScopes.get(scope);
    const remoteScope = scope === "state" ? SYNC_ENTITY_SCOPE : scope;
    let state = scopes.get(scope);
    if (!state) {
      const listeners = new Set<(error: unknown | null) => void>();
      let stopped = false;
      let pending: Promise<void> | undefined;
      let invalidated = false;
      let unversionedInvalidation = false;
      let transcriptEpoch: string | undefined;
      let requiredEntitySequence: number | undefined;
      // Sequence zero is a valid initial baseline. Keep a sentinel until the
      // first pull establishes that the local projection already has that row.
      const isEntityScope = remoteScope === SYNC_ENTITY_SCOPE;
      const latestEntitySequence = isEntityScope
        ? getEntitySequenceCheckpoint(remoteScope, API_SCHEMA_HASH)
        : new EntitySequenceCheckpoint();
      if (
        remoteScope === SYNC_ENTITY_SCOPE &&
        !latestEntitySequence.initialized
      ) {
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
      let recovery = new ReadRecoveryBudget();
      let notifiedVersion: ResourceVersion | undefined;
      const resetReadRecovery = () => {
        recovery = new ReadRecoveryBudget();
      };
      let connectionStatus: ResourceConnectionState = "connecting";
      const retryAbort = new AbortController();
      let stopConnection: (() => void) | undefined;
      const report = (error: unknown | null) => {
        const retryableScopeRead =
          isTransientSyncReadFailure(error) ||
          retryableTranscriptPullConflict(error, scope);
        invalidationBlocked =
          error instanceof WorkspaceMismatchError ||
          (error instanceof ApiError && !retryableScopeRead);
        if (error === null) readFailed = false;
        else readFailed = retryableScopeRead;
        listeners.forEach((listener) => listener(error));
      };
      const retryRead = async <T>(
        request: () => Promise<T>,
        signal?: AbortSignal,
      ): Promise<T> => {
        for (;;) {
          if (recovery.exhaustedError) throw recovery.exhaustedError;
          try {
            return await request();
          } catch (error) {
            if (
              !isTransientSyncReadFailure(error) &&
              !retryableTranscriptPullConflict(error, scope)
            )
              throw error;
            const delay = recovery.nextDelay(error);
            const canRetry =
              connectionStatus === "live" &&
              !document.hidden &&
              navigator.onLine !== false &&
              !stopped &&
              !signal?.aborted;
            if (!canRetry) throw error;
            // The shared budget bounds HTTP failures even while SSE stays live.
            await new Promise<void>((resolve, reject) => {
              const cleanup = () => {
                signal?.removeEventListener("abort", abort);
                retryAbort.signal.removeEventListener("abort", abort);
              };
              const timer = setTimeout(() => {
                cleanup();
                resolve();
              }, delay);
              const abort = () => {
                clearTimeout(timer);
                cleanup();
                reject(new DOMException("Aborted", "AbortError"));
              };
              signal?.addEventListener("abort", abort, { once: true });
              retryAbort.signal.addEventListener("abort", abort, {
                once: true,
              });
              if (signal?.aborted || retryAbort.signal.aborted) abort();
            });
            if (
              connectionStatus !== "live" ||
              document.hidden ||
              navigator.onLine === false ||
              stopped
            )
              throw error;
          }
        }
      };

      const checkpointId =
        remoteScope === SYNC_ENTITY_SCOPE ? "state:entities:checkpoint" : scope;
      const settleCoveredEntityInvalidation = () => {
        if (
          !entitySequenceInvalidationCovered(
            latestEntitySequence.effectiveCoverage(),
            requiredEntitySequence,
            unversionedInvalidation,
          )
        )
          return false;
        invalidated = false;
        requiredEntitySequence = undefined;
        return true;
      };
      const refresh = (signal?: AbortSignal): Promise<void> => {
        if (stopped) return Promise.resolve();
        if (recovery.exhaustedError)
          return Promise.reject(recovery.exhaustedError);
        if (pending) return pending;
        pending = (async () => {
          do {
            const iterationUnversioned = unversionedInvalidation;
            const iterationCanSkipVersioned =
              invalidated &&
              requiredEntitySequence !== undefined &&
              !iterationUnversioned;
            if (signal?.aborted && scopes.get(scope)?.foreground === 0)
              throw new DOMException("Aborted", "AbortError");
            invalidated = false;
            unversionedInvalidation = false;
            if (
              scopes.get(scope)?.foreground === 0 &&
              (document.hidden || navigator.onLine === false)
            )
              throw new TypeError(
                "The device is offline or the page is hidden.",
              );
            let more = true;
            let firstRequest = true;
            const markers =
              remoteScope === SYNC_ENTITY_SCOPE
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
              remoteScope === SYNC_ENTITY_SCOPE && !complete
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
              const cachedTranscriptEpoch =
                previous && scope.startsWith("transcript:")
                  ? readTranscriptMeta(previous.payload)?.epoch
                  : undefined;
              const after =
                hasTranscriptBase &&
                !(
                  cachedTranscriptEpoch !== undefined &&
                  transcriptEpoch !== undefined &&
                  cachedTranscriptEpoch !== transcriptEpoch
                )
                  ? (previous?.seq ?? 0)
                  : 0;
              const pullTranscriptEpoch = transcriptEpoch;
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
              if (
                firstRequest &&
                iterationCanSkipVersioned &&
                remoteScope === SYNC_ENTITY_SCOPE &&
                requiredEntitySequence !== undefined &&
                !iterationUnversioned &&
                !unversionedInvalidation &&
                latestEntitySequence.canUseDurableCheckpoint &&
                !latestEntitySequence.covers(requiredEntitySequence)
              ) {
                // Another tab has its own in-memory registry but can advance
                // this shared IndexedDB checkpoint after persisting the rows.
                if (previous && previous.seq >= requiredEntitySequence) {
                  latestEntitySequence.assignWithinEpoch(previous.seq);
                  latestEntitySequence.markDurableCheckpointValid();
                }
              }
              const pullResetVersion = latestEntitySequence.resetVersion;
              const coveredBeforeDispatch =
                firstRequest &&
                iterationCanSkipVersioned &&
                remoteScope === SYNC_ENTITY_SCOPE &&
                !iterationUnversioned &&
                !unversionedInvalidation &&
                latestEntitySequence.canUseDurableCheckpoint &&
                requiredEntitySequence !== undefined &&
                latestEntitySequence.coversWithInFlight(requiredEntitySequence);
              if (coveredBeforeDispatch) {
                if (
                  remoteScope === SYNC_ENTITY_SCOPE &&
                  requiredEntitySequence !== undefined
                )
                  latestEntitySequence.markInFlightCoverageUsed(
                    requiredEntitySequence,
                  );
                settleCoveredEntityInvalidation();
                more = false;
                break;
              }
              firstRequest = false;
              const result = await retryRead(
                () =>
                  pull(
                    remoteScope,
                    after,
                    remoteScope === SYNC_ENTITY_SCOPE ? ENTITY_BATCH_SIZE : 100,
                    workspaceId,
                    verifyWorkspace,
                    initialHigh,
                    remoteScope === SYNC_ENTITY_SCOPE &&
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
                remoteScope === SYNC_ENTITY_SCOPE &&
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
                remoteScope === SYNC_ENTITY_SCOPE &&
                initialHigh !== undefined &&
                result.initialHigh == null
              )
                throw new Error(
                  "The server omitted the initial sync checkpoint.",
                );
              if (remoteScope === SYNC_ENTITY_SCOPE && result.maxSeq == null)
                throw new Error("The server omitted the sync sequence limit.");
              const entityBatch: SyncDocument[] = [];
              for (const wireDocument of result.documents) {
                if (remoteScope === SYNC_ENTITY_SCOPE) {
                  if (!("op" in wireDocument))
                    throw new Error(
                      "The server returned an invalid entity change.",
                    );
                  entityBatch.push(entityCacheRow(wireDocument));
                  continue;
                }
                if ("op" in wireDocument || wireDocument.id !== remoteScope)
                  throw new Error(
                    "The server returned an invalid projection document.",
                  );
                const document = wireDocument;
                if (scope.startsWith("transcript:")) {
                  let transcriptDocument = document;
                  let incoming: TranscriptSyncPayload | null = document._deleted
                    ? null
                    : JSON.parse(document.payload);
                  if (incoming !== null) {
                    if (
                      isTranscriptDelta(incoming) &&
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
                        (row) =>
                          !("op" in row) && row.id === scope && !row._deleted,
                      );
                      if (!fullDocument || "op" in fullDocument)
                        throw new Error(
                          "Transcript delta has no local item base and the full pull returned no snapshot.",
                        );
                      const fullPayload: TranscriptSyncPayload = JSON.parse(
                        fullDocument.payload,
                      );
                      if (isTranscriptDelta(fullPayload))
                        throw new Error(
                          "Transcript delta has no local item base and the full pull returned an invalid snapshot.",
                        );
                      transcriptDocument = fullDocument;
                      incoming = fullPayload;
                    }
                  }
                  await persistTranscriptProjection(
                    db.projections,
                    scope,
                    transcriptDocument,
                    workspaceId,
                    incoming,
                    undefined,
                    () => pullTranscriptEpoch === transcriptEpoch && !stopped,
                    pullTranscriptEpoch,
                  );
                  continue;
                }
                await persistProjection(db.projections, {
                  ...document,
                  id: scope,
                });
              }
              if (remoteScope === SYNC_ENTITY_SCOPE) {
                const applied = await withEntityProjectionWrite(
                  db.projections.database.name,
                  async () => {
                    const isCurrent = () =>
                      latestEntitySequence.isSameResetVersion(pullResetVersion);
                    if (!isCurrent()) return false;
                    if (initialHigh !== undefined) {
                      await persistProjection(db.projections, {
                        id: "state:entities:initial",
                        payload: "{}",
                        seq: requiredSyncNumber(
                          result.initialHigh,
                          "initial checkpoint",
                        ),
                      });
                      if (!isCurrent()) return false;
                    }
                    await persistProjectionBatch(
                      db.projections,
                      entityBatch,
                      isCurrent,
                    );
                    if (!isCurrent()) return false;
                    await persistProjection(db.projections, {
                      id: checkpointId,
                      payload: JSON.stringify({
                        initialHigh: result.initialHigh,
                      }),
                      seq: Math.max(
                        latestEntitySequence.value,
                        result.checkpoint.seq,
                      ),
                    });
                    if (!isCurrent()) return false;
                    latestEntitySequence.assignWithinEpoch(
                      result.checkpoint.seq,
                    );
                    latestEntitySequence.markDurableCheckpointValid();
                    if (!readyPublished) {
                      await persistProjection(db.projections, {
                        id: "state:entities:ready",
                        payload: "ready",
                        seq: 1,
                      });
                      if (!isCurrent()) return false;
                      readyPublished = true;
                    }
                    return true;
                  },
                );
                if (!applied) {
                  more = true;
                  continue;
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
            settleCoveredEntityInvalidation();
            report(null);
          } while (invalidated && !stopped);
          if (!stopped && remoteScope === SYNC_ENTITY_SCOPE) {
            const completedResetVersion = latestEntitySequence.resetVersion;
            await withEntityProjectionWrite(
              db.projections.database.name,
              async () => {
                const isCurrent = () =>
                  !stopped &&
                  !invalidated &&
                  latestEntitySequence.isSameResetVersion(
                    completedResetVersion,
                  );
                if (!isCurrent()) return;
                await persistProjection(db.projections, {
                  id: "state:entities:complete",
                  payload: "complete",
                  seq: Date.now(),
                });
                if (!isCurrent()) return;
                await persistProjection(db.projections, {
                  id: "state:entities:ready",
                  payload: "ready",
                  seq: resetReadySeq ?? 1,
                });
              },
            );
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
      const stopCoverageFallback = isEntityScope
        ? latestEntitySequence.onInFlightCoverageInvalidated(() => {
            // A skipped versioned frame can no longer be satisfied by the
            // mutation response. Run one ordinary refresh from the durable cursor.
            invalidated = true;
            requiredEntitySequence = undefined;
            void refresh().catch(() => {});
          })
        : undefined;
      const refreshAfterCurrent = () =>
        refreshAfterCurrentPull(pending, refresh);
      const transcriptId = scope.startsWith("transcript:")
        ? scope.slice(11)
        : null;
      let stopInvalidation: (() => void) | undefined;
      const invalidate = (version?: ResourceVersion) => {
        // A stream hint cannot authorize another read after a permanent rejection.
        // Explicit refresh still uses the normal workspace and HTTP checks.
        if (invalidationBlocked) return;
        if (transcriptId && version) transcriptEpoch = version.epoch;
        if (transcriptId && version?.transcriptSequence !== undefined) {
          const through = version.transcriptSequence;
          void db.projections.storageInstance
            .findDocumentsById([scope], true)
            .then(([row]) => {
              const meta =
                row && !row._deleted ? readTranscriptMeta(row.payload) : null;
              if (
                !row ||
                !meta ||
                row.seq < through ||
                (meta.epoch !== undefined &&
                  meta.epoch !== version.epoch &&
                  !version.cursor)
              )
                invalidate();
            })
            .catch(() => invalidate());
          return;
        }
        if (remoteScope === SYNC_ENTITY_SCOPE && version) {
          latestEntitySequence.observeEpoch(version.epoch);
          if (version.entitySequenceReset) {
            latestEntitySequence.reset();
            requiredEntitySequence = undefined;
          }
        }
        if (
          remoteScope === SYNC_ENTITY_SCOPE &&
          version?.entitySequence !== undefined
        ) {
          if (
            latestEntitySequence.canUseDurableCheckpoint &&
            latestEntitySequence.covers(version.entitySequence)
          )
            return;
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
      const notifyInvalidation = (version?: ResourceVersion) => {
        if (
          !version ||
          !notifiedVersion ||
          version.epoch !== notifiedVersion.epoch ||
          version.revision > notifiedVersion.revision
        ) {
          resetReadRecovery();
          if (version) notifiedVersion = version;
        }
        invalidate(version);
      };
      const activateInvalidation = () => {
        if (stopInvalidation) return;
        stopInvalidation = transcriptId
          ? watchTranscriptInvalidations(transcriptId, notifyInvalidation)
          : watchSyncInvalidations(
              notifyInvalidation,
              remoteScope === SYNC_ENTITY_SCOPE ? "entities" : "legacy",
            );
        stopConnection = watchResourceConnection((status) => {
          const reconnected = status === "live" && connectionStatus !== "live";
          connectionStatus = status;
          if (
            reconnected &&
            (readFailed || recovery.exhaustedError) &&
            !invalidationBlocked
          ) {
            resetReadRecovery();
            void refreshAfterCurrent().catch(() => {});
          }
        });
        stopResume = onResume(() => {
          if (readFailed && !invalidationBlocked)
            void refreshAfterCurrent().catch(() => {});
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
        resetReadRecovery,
        activateInvalidation,
        stop: async () => {
          stopped = true;
          retryAbort.abort();
          stopInvalidation?.();
          stopCoverageFallback?.();
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

  async function watchStateProjection(
    accept: (payload: StateProjectionPayload) => void,
    fail: (e: unknown) => void,
    acceptUiPreferences?: (preferences: UiPreferenceProjection) => void,
  ) {
    const { db, state, release } = await acquireProjection("state");
    state.listeners.add(fail);
    let stopEntities: (() => void) | undefined;
    try {
      stopEntities = await observeEntityProjection(
        db.projections,
        accept,
        fail,
        acceptUiPreferences,
      );
      // Start the first incomplete projection pull as soon as the stream has been
      // subscribed, without waiting for its baseline frame. A completed local
      // cache already supplies content and needs no speculative pull.
      const [complete] = await db.projections.storageInstance.findDocumentsById(
        ["state:entities:complete"],
        true,
      );
      if (!complete || complete._deleted) void state.refresh().catch(() => {});
    } catch (error) {
      stopEntities?.();
      state.listeners.delete(fail);
      await release();
      throw error;
    }
    return () => {
      stopEntities?.();
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
        cacheStoredTranscriptAfterWrite(
          db.projections,
          workspaceId,
          scope,
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

  /** Pull and return the current entity projection for state reconciliation. */
  async function refreshProjection(
    options: {
      afterCurrentPull?: boolean;
      onWorkspaceId?: (workspaceId: string) => void;
    } = {},
  ): Promise<Snapshot> {
    return readProjection(options, true);
  }

  async function readProjection(
    options: {
      afterCurrentPull?: boolean;
      onWorkspaceId?: (workspaceId: string) => void;
    } = {},
    explicitRetry = false,
  ): Promise<Snapshot> {
    const handle = await acquireProjection("state");
    try {
      if (explicitRetry) handle.state.resetReadRecovery();
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
      options.onWorkspaceId?.(handle.workspaceId);
      return snapshot;
    } finally {
      await handle.release();
    }
  }

  const TRANSCRIPT_PREFETCH_LIMIT = 1;

  let prefetches = 0;

  const pendingPrefetches = new Map<string, Promise<boolean>>();

  /** Refresh one persisted chat without keeping a background subscription alive. */
  function prefetchTranscript(
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
              cacheStoredTranscriptAfterWrite(
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

  async function startDraftReplication(
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
    const { db, workspaceId, verifyWorkspace, draftConflicts } =
      await syncDatabase();
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
    // Ordinary complete draft batches enter RxDB's master stream. Their accepted
    // watermark proves continuity; RxDB owns durable checkpoints and conflicts.
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
    let draftEpoch: string | undefined;
    let acceptedDraftSequence: number | undefined;
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
      const activeDown =
        replication.internalReplicationState?.events.active.down;
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
                acceptedDraftSequence = Math.max(
                  acceptedDraftSequence ?? 0,
                  result.checkpoint.seq,
                );
                pullContinues = result.documents.length >= batchSize;
                const documents = result.documents;
                if (
                  !documents.every(
                    (
                      document,
                    ): document is components["schemas"]["SyncDocument"] =>
                      "payload" in document,
                  )
                )
                  throw new TypeError("Expected a draft document");
                return { documents, checkpoint: result.checkpoint };
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
              const requestRows = rows.map((row) => {
                const { _deleted, ...newDocumentState } = row.newDocumentState;
                const assumedMasterState = row.assumedMasterState;
                if (!assumedMasterState) {
                  return {
                    ...row,
                    newDocumentState: {
                      ...newDocumentState,
                      deleted: _deleted,
                    },
                  };
                }
                const { _deleted: assumedDeleted, ...assumedState } =
                  assumedMasterState;
                return {
                  ...row,
                  newDocumentState: { ...newDocumentState, deleted: _deleted },
                  assumedMasterState: {
                    ...assumedState,
                    deleted: assumedDeleted,
                  },
                };
              });
              const conflicts = await syncPost(
                "/api/sync/drafts",
                { rows: requestRows },
                { workspaceId },
              );
              draftConflicts.acknowledge(rows, conflicts);
              return conflicts;
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
          if (direction !== "push" && acceptedDraftSequence !== undefined)
            triggerPull();
        }),
      ];
      replication = current;
      return current;
    };
    createReplication();
    const waitForDownstreamInitialSync = async (
      current: typeof replication,
    ) => {
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
      acceptedDraftSequence = undefined;
      pullTriggered = true;
      servePendingPull();
    };
    const stopInvalidation = watchSyncInvalidations((version) => {
      const batch = version?.draftChanges;
      if (version?.reason !== "change" || version.epoch !== draftEpoch) {
        draftEpoch = version?.epoch;
        triggerPull();
        return;
      }
      if (
        acceptedDraftSequence !== undefined &&
        version.revision <= acceptedDraftSequence
      )
        return;
      if (
        !batch ||
        acceptedDraftSequence === undefined ||
        batch.after > acceptedDraftSequence ||
        batch.through !== version.revision ||
        batch.through <= batch.after ||
        batch.documents.at(-1)?.seq !== batch.through
      ) {
        triggerPull();
        return;
      }
      const after = acceptedDraftSequence;
      replication.emitEvent({
        documents: batch.documents.filter((row) => row.seq > after),
        checkpoint: { seq: batch.through },
      });
      acceptedDraftSequence = batch.through;
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
    let recovery = new ReadRecoveryBudget();
    let connectionStatus: ResourceConnectionState = "connecting";
    let retryTimer: ReturnType<typeof setTimeout> | undefined;
    const clearRetry = () => {
      clearTimeout(retryTimer);
      retryTimer = undefined;
    };
    const onFailure = (error: unknown | null) => {
      if (stopped) return;
      report(error);
      if (error === null || error instanceof ReadRecoveryExhaustedError) {
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
      let delay: number;
      try {
        delay = recovery.nextDelay(error);
      } catch (exhausted) {
        report(exhausted);
        clearRetry();
        return;
      }
      retryTimer = setTimeout(() => {
        retryTimer = undefined;
        if (stopped || hasData) return;
        if (attached) void retryBeforeData().catch(onFailure);
        else void connect();
      }, delay);
    };
    const connect = async () => {
      if (stopped || connecting || attached || recovery.exhaustedError) return;
      clearRetry();
      connecting = true;
      try {
        const stop = await watch((value) => {
          if (!stopped) {
            if (value !== null) {
              hasData = true;
              clearRetry();
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
      const reconnected = status === "live" && connectionStatus !== "live";
      connectionStatus = status;
      if (reconnected && !attached) {
        recovery = new ReadRecoveryBudget();
        void connect();
      }
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

  function subscribeStateProjection(
    accept: (payload: StateProjectionPayload) => void,
    report: (error: unknown | null) => void,
    acceptUiPreferences?: (preferences: UiPreferenceProjection) => void,
  ) {
    return subscribeProjection(
      (next, fail) => watchStateProjection(next, fail, acceptUiPreferences),
      accept,
      report,
      () => readProjection(),
    );
  }

  function subscribeTranscriptProjection(
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
  return {
    syncDatabase,
    persistResourceEntityChanges,
    readSubscriptionCursor,
    watchSyncInvalidations,
    persistProjection,
    refreshProjection,
    TRANSCRIPT_PREFETCH_LIMIT,
    prefetchTranscript,
    startDraftReplication,
    subscribeStateProjection,
    subscribeTranscriptProjection,
  };
}
const defaultSyncClient = createSyncClient({
  api: {
    syncGet,
    syncPost,
    saved,
    save,
    setWorkspace,
    isApiSchemaMismatch,
    onMatchingApiSchemaResponse,
  },
  resources: {
    watchResourceChanges: (...args: Parameters<typeof watchResourceChanges>) =>
      watchResourceChanges(...args),
    watchResourceConnection: (
      ...args: Parameters<typeof watchResourceConnection>
    ) => watchResourceConnection(...args),
  },
  sequences: { getEntitySequenceCheckpoint, setEntitySequenceProjection },
  transcripts: {
    clearTranscript,
    cacheTranscriptValue,
    peekTranscript,
    patchTranscriptValue,
    subscribeTranscript,
  },
  databaseSuffix: serverDatabaseSuffix,
});
export const {
  syncDatabase,
  persistResourceEntityChanges,
  readSubscriptionCursor,
  watchSyncInvalidations,
  persistProjection,
  refreshProjection,
  TRANSCRIPT_PREFETCH_LIMIT,
  prefetchTranscript,
  startDraftReplication,
  subscribeStateProjection,
  subscribeTranscriptProjection,
} = defaultSyncClient;
