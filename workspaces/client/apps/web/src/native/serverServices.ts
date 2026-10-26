import { createNativeApi } from "./api";
import { createNativeSync } from "./sync";
import { createNativeSubscriptions } from "./subscriptions";
import { createNativeOutbox } from "./outbox";
import { createNativeDrafts } from "./drafts";
import { createTranscriptCache } from "../sync/transcriptCache";
import { createTokenRateCache } from "../usage/tokenRate";
import { createAccountLimitsCache } from "../usage/accountUsageCache";
import { createSessionCostCache } from "../usage/sessionCostCacheCore";
import { createChatCreationStore } from "../chatCreationStore";
import { createEntitySequenceScope } from "../sync/entitySequence";
import { createNativeActivity } from "./services/activity";
import { createNativeResourceReads } from "./services/resourceReads";
import { createNativeLocalDrafts } from "./services/localDrafts";
import { createNativeFileServices } from "./services/files";
import { createNativeMediaServices } from "./services/media";
import { createNativePreferenceDomain } from "./services/preferenceDomain";
import { createNativeUploads } from "./services/uploads";
import { createNativeAttachmentSelections } from "./attachmentSelection";
import { createNativeServiceLifecycle } from "./services/lifecycle";
import type {
  NativeServiceReady,
  NativeServiceOwner,
  NativeServiceOptions,
} from "./services/types";
import type { NativeServerContext, NativeSubscriptionEvent } from "./client";
import type { AppPreferences } from "../servers/appPreferences";
import { setPreferenceWriteStatus } from "../sync/uiPreferenceStatus";
import type {
  SidebarBackend,
  SidebarServices,
} from "../components/sidebar/services";

/** Every port closes over one host context, never the selected screen or origin. */
export function createNativeServerServices(
  selected: NativeServiceOwner,
  ready: NativeServiceReady,
  options: NativeServiceOptions,
) {
  if (
    !selected.id ||
    selected.id !== ready.context.serverId ||
    selected.origin !== ready.context.origin ||
    (selected.credentialId ?? null) !== ready.context.credentialId ||
    (selected.workspaceId !== undefined &&
      selected.workspaceId !== ready.context.workspaceId)
  )
    throw new Error("The native service owner does not match its context.");
  ready.storage.assertOwner(ready.context);
  const native = ready.context;
  const owner = Object.freeze({
    id: native.serverId,
    origin: native.origin,
    credentialId: native.credentialId ?? undefined,
    workspaceId: native.workspaceId,
    label: selected.label,
    ...(selected.alias === undefined ? {} : { alias: selected.alias }),
  });
  let closed = false;
  let initialization: Promise<void> | undefined;
  let initialized = false;
  let closing: Promise<void> | undefined;
  const ensureOpen = () => {
    if (closed) throw new Error("The native server services are closed.");
  };
  const context: NativeServerContext = Object.freeze({
    handle: native.handle,
    serverId: native.serverId,
    workspaceId: native.workspaceId,
    origin: native.origin,
    credentialId: native.credentialId,
    async request<T>(method: string, params: unknown, id?: string) {
      ensureOpen();
      if (method === "api.request" || method === "api.prepareSession")
        await initialize();
      ensureOpen();
      return native.request<T>(method, params, id);
    },
    subscribe<T>(
      topic: string,
      params: Record<string, unknown>,
      receive: (event: NativeSubscriptionEvent<T>) => void,
    ) {
      ensureOpen();
      return native.subscribe<T>(topic, params, receive);
    },
  });
  const source = ready.storage;
  const storage = {
    get length() {
      return source.length;
    },
    key: (index: number) => source.key(index),
    getItem: (key: string) => source.getItem(key),
    setItem(key: string, value: string) {
      ensureOpen();
      source.setItem(key, value);
    },
    removeItem(key: string) {
      ensureOpen();
      source.removeItem(key);
    },
    clear() {
      ensureOpen();
      source.clear();
    },
  } satisfies Storage;
  const lifecycle = createNativeServiceLifecycle(native, options.report);
  const preferences = createNativePreferenceDomain(
    native,
    source,
    lifecycle,
    options.report,
    (sync) => {
      setPreferenceWriteStatus(storage, "user", sync.user);
      setPreferenceWriteStatus(storage, "server", sync.server);
    },
  );
  const uploads = createNativeUploads(context, lifecycle, options.report);
  const attachmentSelections = createNativeAttachmentSelections(
    context,
    uploads,
    options.attachmentHost,
  );
  const localDrafts = createNativeLocalDrafts(native, source);
  const activity = createNativeActivity();
  const base = createNativeApi(context, storage);
  const resources = createNativeSubscriptions(Promise.resolve(context));
  const sync = createNativeSync(Promise.resolve(context), resources);
  const outbox = createNativeOutbox(context);
  const files = createNativeFileServices(context, {
    releasePreview: (handle) => native.request("preview.release", { handle }),
    stopPreviewPlayback: (playback) =>
      native.request("media.stopPlayback", { playback }),
  });
  const media = createNativeMediaServices(context);
  const tokenRates = createTokenRateCache();
  tokenRates.configureTokenRateStream(resources.watchTokenRateEvents);
  const flush = async () => {
    await preferences.flush();
    await localDrafts.flush();
    await source.flush();
  };
  const initialize = () => {
    ensureOpen();
    if (!initialization)
      initialization = Promise.all([
        preferences.initialize(),
        uploads.initialize(),
      ])
        .then(() => {
          ensureOpen();
          initialized = true;
        })
        .catch((error) => {
          initialization = undefined;
          if (!closed) options.report(error);
          throw error;
        });
    return initialization;
  };
  const post: typeof base.post = async (path, body, requestOptions) => {
    ensureOpen();
    await flush();
    await initialize();
    const finish = activity.beginServerActivity();
    try {
      return await base.post(path, body, requestOptions);
    } finally {
      finish();
    }
  };
  const api = {
    ...base,
    post,
    syncPost: post,
    async prepareSession() {
      ensureOpen();
      await flush();
      await initialize();
      return base.prepareSession();
    },
  };
  const backend = {
    ownerId: owner.id,
    storage,
    resources,
    post,
    saved: api.saved,
    save: api.save,
    editPreference: preferences.editPreference,
    viewPreference: preferences.editPreference,
    subscribePreference: preferences.subscribePreference,
    preferenceFields: preferences.preferenceFields,
    adoptPreferenceFields: preferences.adoptPreferenceFields,
  } satisfies SidebarBackend;
  const sidebarServices = {
    cache: backend,
    owner: () => backend,
    byOwner(id: string) {
      if (id !== owner.id)
        throw new Error("The sidebar belongs to another native server.");
      return backend;
    },
    serverForChat: () => owner.id,
    available: () => initialized && !closed,
    unavailable: () =>
      initialized && !closed ? null : "Wait for this server's native services.",
  } satisfies SidebarServices;
  const preferenceStops = new Set<() => void>();
  const appPreferences: AppPreferences = {
    editPreference: preferences.editPreference,
    projectionBridge: null,
    publishWorkspace(workspaceId) {
      ensureOpen();
      if (workspaceId !== native.workspaceId)
        throw new Error("The native workspace does not match this screen.");
    },
  };
  const preferenceEvents = {
    subscribe(listener: (key: string | null) => void) {
      ensureOpen();
      const release = source.subscribe(listener);
      const stop = () => {
        preferenceStops.delete(stop);
        release();
      };
      preferenceStops.add(stop);
      return stop;
    },
  };
  return {
    owner,
    context: Promise.resolve(context),
    api,
    sync,
    resources,
    outbox,
    storage,
    uploads,
    attachmentSelections,
    activity,
    sidebarServices,
    ...files,
    ...media,
    preferenceEvents,
    appPreferences,
    transcripts: createTranscriptCache(),
    tokenRates,
    sequences: createEntitySequenceScope(),
    accountLimits: createAccountLimitsCache(storage),
    sessionCosts: createSessionCostCache(storage),
    chatCreations: createChatCreationStore(storage),
    localDrafts,
    storageEventKey: (event: Pick<StorageEvent, "key">) => event.key,
    watchResourceReads: createNativeResourceReads(resources),
    useDrafts: createNativeDrafts(context),
    ready: initialize,
    flush,
    close() {
      if (closing) return closing;
      closed = true;
      preferences.stop();
      localDrafts.stop();
      for (const stop of preferenceStops) stop();
      const pendingInitialization = initialization?.catch(() => {});
      closing = (async () => {
        const results = await Promise.allSettled([
          pendingInitialization,
          flush(),
          lifecycle.dispose(),
          resources.dispose(),
          sync.dispose(),
          outbox.dispose(),
          uploads.close(),
          files.previews.close(),
        ]);
        const failures = results
          .filter(
            (result): result is PromiseRejectedResult =>
              result.status === "rejected",
          )
          .map((result) => result.reason);
        if (failures.length)
          throw new AggregateError(
            failures,
            "The native server services did not close.",
          );
      })();
      return closing;
    },
  };
}

export type ServerServices = ReturnType<typeof createNativeServerServices>;
