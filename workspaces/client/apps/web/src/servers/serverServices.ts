import { watchServerName } from "./serverNames";
import { createSessionCostCache } from "../usage/sessionCostCache";
import { createUploads, UPLOAD_DATABASE_NAME } from "../sync/uploads";
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
import { nativeProjectionStore } from "../sync/nativeProjections";
import { createServerSubscriptions } from "../sync/subscriptions/server";
import { createTranscriptCache } from "../sync/transcriptCache";
import { createTokenRateCache } from "../usage/tokenRate";
import { createEntitySequenceScope } from "../sync/entitySequence";
import { createApiScope } from "../api";
import { hasNativeClient, nativeRequest } from "../native/client";
import { nativeFetch } from "../native/http";
import { preferenceStorageForServer } from "../sync/uiPreferenceStore";
import { createServerActivity } from "./activity";
import { createMutationJournal } from "./mutationIdentity";
import type { StudioServer } from "./registry";
import { createServerTransport } from "./serverTransport";
import { scopedStorage } from "./storage";
import { serverCredentialAdapter } from "./transport";

/** Services belong to a server, never to the currently visible chat. */
export function createServerServices(server: StudioServer) {
  const transport = createServerTransport(server, (owner, request) => {
    if (owner.id === "local") {
      if (hasNativeClient()) return nativeFetch(request, owner.origin);
      if (!window.codexDesktop) return fetch(request);
    }
    return serverCredentialAdapter().fetch(owner, request);
  });
  const storage = preferenceStorageForServer(transport.owner.id);
  // The native core stores the projections of the local server; every other server keeps the page cache.
  const localNativeClient = () =>
    transport.owner.id === "local" && hasNativeClient();
  // Usage caches persist only in the local native core; other servers keep memory only.
  const usage: UsagePersistence = () =>
    localNativeClient() ? nativeRequest : undefined;
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
      nativeCore: localNativeClient,
    });
    sync = createSyncClient({
      api,
      resources,
      sequences,
      transcripts,
      projectionStore: () =>
        localNativeClient() ? nativeProjectionStore : undefined,
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
  const outbox: OutboxService = localNativeClient()
    ? createNativeOutbox(() => synchronization().sync.outboxDatabase())
    : createOutbox({
        api,
        syncDatabase: () => synchronization().sync.outboxDatabase(),
        storage,
      });
  const localDrafts = createLocalDraftStorage(
    storage,
    storageNameForServer(transport.owner.id, LOCAL_DRAFT_DATABASE_NAME),
  );
  const uploads = createUploads({
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
    audio,
    sessionCosts,
    chatCreations: createChatCreationStore(storage),
    recordings: createRecordingStorage(
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
      return draftsForServer(transport.owner.id === "local");
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
