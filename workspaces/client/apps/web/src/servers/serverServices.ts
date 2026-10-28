import { watchServerName } from "./serverNames";
import { createSessionCostCache } from "../usage/sessionCostCache";
import { createUploads, UPLOAD_DATABASE_NAME } from "../sync/uploads";
import { createNativeUploads } from "../native/uploads";
import { createNativeRecordings } from "../native/recordings";
import { createServerAudio } from "../native/serverAudio";
import {
  createRecordingStorage,
  DICTATION_DATABASE_NAME,
} from "../dictation/storage";
import {
  createLocalDraftStorage,
  LOCAL_DRAFT_DATABASE_NAME,
} from "../sync/localDraft";
import { storageNameForServer } from "./environment";
import { serverStoragePrefix, createStorageEventKey } from "./storage";
import { createChatCreationStore } from "../chatCreation";
import { createFileActions } from "../fileActions";
import { createFileDialogs } from "../native/fileDialogs";
import { watchResourceReads } from "../sync/subscriptions/reads";
import { createAccountLimitsCache } from "../usage/accountUsage";
import type { UsagePersistence } from "../usage/usageRequest";
import { draftsForServer } from "../native/draftSource";
import { createOutbox, type OutboxService } from "../sync/send";
import { createNativeOutbox } from "../native/outbox";
import { createSyncClient } from "../sync/client";
import { createNativeProjectionStore } from "../sync/nativeProjections";
import {
  importLegacyUploads,
  openLegacyUploads,
} from "../native/legacyUploads";
import {
  importLegacyRecordings,
  openLegacyRecordings,
} from "../native/legacyRecordings";
import { createServerSubscriptions } from "../sync/subscriptions/server";
import { createTranscriptCache } from "../sync/transcriptCache";
import { createTokenRateCache } from "../usage/tokenRate";
import { createEntitySequenceScope } from "../sync/entitySequence";
import { createApiScope } from "../api";
import {
  coreForServer,
  hasNativeClient,
  type CoreClient,
} from "../native/client";
import { nativeFetch } from "../native/http";
import { preferenceStorageForServer } from "../sync/uiPreferenceStore";
import { createServerActivity } from "./activity";
import { createMutationJournal } from "./mutationIdentity";
import type { StudioServer } from "./registry";
import { createServerTransport } from "./serverTransport";
import { scopedStorage } from "./storage";
import { serverCredentialAdapter } from "./transport";

/** The queue, files, recordings and drafts that the core of one server keeps. */
export function coreStores(
  core: CoreClient,
  outboxDatabase: Parameters<typeof createNativeOutbox>[0],
) {
  return {
    outbox: createNativeOutbox(outboxDatabase, core),
    uploads: createNativeUploads(core.request, () =>
      importLegacyUploads(core.request, openLegacyUploads(core.server)),
    ),
    recordings: createNativeRecordings(core.request, () =>
      importLegacyRecordings(core.request, openLegacyRecordings(core.server)),
    ),
    useDrafts: draftsForServer(core),
  };
}

/** Services belong to a server, never to the currently visible chat. */
export function createServerServices(server: StudioServer) {
  // Each server has its own client core: the desktop keeps one per paired
  // server, and a browser page uses the cores that its server hosts for the device.
  const core = hasNativeClient() ? coreForServer(server.id) : undefined;
  const transport = createServerTransport(server, (owner, request) => {
    if (core) return nativeFetch(request, owner.origin, core);
    if (owner.id === "local" && !window.codexDesktop) return fetch(request);
    return serverCredentialAdapter().fetch(owner, request);
  });
  const storage = preferenceStorageForServer(transport.owner.id);
  const projections = core && createNativeProjectionStore(core.request);
  const usage: UsagePersistence = () => core?.request;
  const accountLimits = createAccountLimitsCache(usage);
  const sequences = createEntitySequenceScope();
  const activity = createServerActivity();
  const api = createApiScope({
    origin: transport.owner.origin,
    fetch: transport.fetch,
    localStorage: storage,
    sessionStorage: scopedStorage(
      () => globalThis.sessionStorage,
      `:server:${transport.owner.id}:`,
    ),
    beginMutation: createMutationJournal(transport.owner.id, storage),
    beginActivity: activity.beginServerActivity,
  });
  const audio = createServerAudio();
  const sessionCosts = createSessionCostCache(usage);
  const transcripts = createTranscriptCache();
  const tokenRates = createTokenRateCache();
  let runtime:
    | {
        sync: ReturnType<typeof createSyncClient>;
        resources: ReturnType<typeof createServerSubscriptions>;
      }
    | undefined;
  let stopName = () => {};
  const synchronization = () => {
    if (runtime) return runtime;
    let sync!: ReturnType<typeof createSyncClient>;
    const resources = createServerSubscriptions({
      api,
      tokenRates,
      sync: () => sync,
      owner: transport.owner,
      core,
    });
    sync = createSyncClient({
      api,
      resources,
      sequences,
      transcripts,
      projectionStore: () => projections,
      databaseSuffix:
        transport.owner.id === "local"
          ? ""
          : "server" +
            Array.from(new TextEncoder().encode(transport.owner.id), (byte) =>
              byte.toString(16).padStart(2, "0"),
            ).join(""),
    });
    runtime = { sync, resources };
    if (transport.owner.id !== "local")
      stopName = watchServerName(transport.owner, resources);
    return runtime;
  };
  const stores =
    core && coreStores(core, () => synchronization().sync.outboxDatabase());
  const outbox: OutboxService = stores
    ? stores.outbox
    : createOutbox({
        api,
        syncDatabase: () => synchronization().sync.outboxDatabase(),
        storage,
      });
  const localDrafts = createLocalDraftStorage(
    storage,
    storageNameForServer(transport.owner.id, LOCAL_DRAFT_DATABASE_NAME),
  );
  // The core of the server owns its uploads and recordings.
  const uploads = stores
    ? stores.uploads
    : createUploads({
        databaseName: storageNameForServer(
          transport.owner.id,
          UPLOAD_DATABASE_NAME,
        ),
        storage,
        syncGet: api.syncGet,
        syncPost: api.syncPost,
        refreshSession: api.refreshSession,
        writeLocalDraft: localDrafts.writeLocalDraft,
        syncDatabase: () => synchronization().sync.syncDatabase(),
      });
  let initialization: Promise<void> | undefined;
  async function ready() {
    if (!initialization) {
      initialization = Promise.all([
        api.get("/api/sync/identity"),
        api.refreshSession(),
      ])
        .then(([identity]) => {
          api.setWorkspace(identity.workspaceId);
        })
        .catch((error) => {
          initialization = undefined;
          throw error;
        });
    }
    await initialization;
  }
  return {
    owner: transport.owner,
    /** The client core of this server, or undefined without a client core. */
    core,
    audio,
    sessionCosts,
    chatCreations: createChatCreationStore(storage),
    recordings: stores
      ? stores.recordings
      : createRecordingStorage(
          storageNameForServer(transport.owner.id, DICTATION_DATABASE_NAME),
        ),
    localDrafts,
    uploads,
    storageEventKey: createStorageEventKey(
      serverStoragePrefix(transport.owner.id),
    ),
    fileDialogs: createFileDialogs(transport.owner.id),
    fileActions: createFileActions(transport.owner.id),
    accountLimits,
    watchResourceReads: ((refs, read, failed, options) =>
      watchResourceReads(refs, read, failed, {
        ...options,
        events: synchronization().resources,
      })) satisfies typeof watchResourceReads,
    activity,
    sequences,
    transcripts,
    outbox,
    get useDrafts() {
      return stores?.useDrafts ?? draftsForServer(undefined);
    },
    tokenRates,
    get sync() {
      return synchronization().sync;
    },
    get resources() {
      return synchronization().resources;
    },
    api,
    storage,
    ready,
    close() {
      audio.close();
      stopName();
      runtime?.resources.dispose();
      transport.close();
    },
  };
}
export type ServerServices = ReturnType<typeof createServerServices>;
