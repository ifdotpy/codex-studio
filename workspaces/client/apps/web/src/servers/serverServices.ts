import { createAccountLimitsCache } from "../usage/accountUsage";
import { createBrowserDrafts } from "../sync/drafts";
import { createDraftJournal } from "../sync/draftJournal";
import { createDraftStorage } from "../sync/draftStorage";
import { useNativeDrafts } from "../native/useNativeDrafts";
import { createOutbox } from "../sync/send";
import { createSyncClient } from "../sync/client";
import { createResourceEvents } from "../sync/resourceEvents";
import { SignedEventSource } from "./eventSource";
import { createTranscriptCache } from "../sync/transcriptCache";
import { createTokenRateCache } from "../usage/tokenRate";
import { createEntitySequenceScope } from "../sync/entitySequence";
import { createApiScope } from "../api";
import { hasNativeClient } from "../native/client";
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
  const accountLimits = createAccountLimitsCache(storage);
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
    entityCheckpoint: sequences.getEntitySequenceCheckpointForWorkspace,
  });
  const transcripts = createTranscriptCache();
  const tokenRates = createTokenRateCache();
  let runtime:
    | {
        sync: ReturnType<typeof createSyncClient>;
        resources: ReturnType<typeof createResourceEvents>;
      }
    | undefined;
  const synchronization = () => {
    if (runtime) return runtime;
    let sync!: ReturnType<typeof createSyncClient>;
    const resources = createResourceEvents({
      api,
      tokenRates,
      sync: {
        syncDatabase: () => sync.syncDatabase(),
        persistResourceEntityChanges: (...args) =>
          sync.persistResourceEntityChanges(...args),
      },
      openEvents: (url) =>
        new SignedEventSource(
          new URL(url, transport.owner.origin).href,
          api.fetchApiRequest,
        ),
      storageName: (name) =>
        transport.owner.id === "local"
          ? name
          : `${name}:server:${transport.owner.id}`,
    });
    sync = createSyncClient({
      api,
      resources,
      sequences,
      transcripts,
      databaseSuffix:
        transport.owner.id === "local"
          ? ""
          : "server" +
            Array.from(new TextEncoder().encode(transport.owner.id), (byte) =>
              byte.toString(16).padStart(2, "0"),
            ).join(""),
    });
    runtime = { sync, resources };
    return runtime;
  };
  const outbox = createOutbox({
    api,
    syncDatabase: () => synchronization().sync.syncDatabase(),
    storage,
  });
  let browserDrafts: ReturnType<typeof createBrowserDrafts> | undefined;
  const drafts = () => {
    if (transport.owner.id === "local" && hasNativeClient())
      return useNativeDrafts;
    return (browserDrafts ??= createBrowserDrafts({
      api,
      sync: {
        syncDatabase: () => synchronization().sync.syncDatabase(),
        startDraftReplication: (...args) =>
          synchronization().sync.startDraftReplication(...args),
      },
      journal: createDraftJournal(storage),
      storage: createDraftStorage(storage),
    }));
  };
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
    accountLimits,
    activity,
    sequences,
    transcripts,
    outbox,
    get useDrafts() {
      return drafts();
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
      runtime?.resources.dispose();
      transport.close();
    },
  };
}
export type ServerServices = ReturnType<typeof createServerServices>;
