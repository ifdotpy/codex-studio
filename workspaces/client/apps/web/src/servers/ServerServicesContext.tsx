import * as defaultSessionCosts from "../usage/sessionCostCache";
import * as defaultUploads from "../sync/uploads";
import { defaultAttachmentSelections } from "../browserAttachmentSelection";
import { defaultServerAudio } from "../native/serverAudio";
import * as defaultRecordings from "../dictation/storage";
import * as defaultLocalDrafts from "../sync/localDraft";
import { serverStorageEventKey } from "./storage";
import * as defaultChatCreations from "../chatCreation";
import { serverLocalStorage } from "./storage";
import * as defaultFileActions from "../fileActions";
import * as defaultFileDialogs from "../native/fileDialogs";
import { watchResourceReads } from "../sync/subscriptions/reads";
import * as defaultAccountLimits from "../usage/accountUsage";
import * as defaultActivity from "./activity";
import { useSyncedDrafts } from "../sync/drafts";
import * as defaultOutbox from "../sync/send";
import {
  createContext,
  useContext,
  useMemo,
  useState,
  Fragment,
  type ReactNode,
} from "react";
import {
  createSidebarServices,
  localSidebarServices,
} from "../components/sidebar/services";
import { serverSidebarBackend } from "./serverSidebarBackend";
import * as defaultApi from "../api";
import * as defaultSync from "../sync/client";
import { defaultSubscriptions as defaultResources } from "../sync/subscriptions/events";
import * as defaultTranscripts from "../sync/transcriptCache";
import * as defaultTokenRates from "../usage/tokenRate";
import type { ServerServices } from "./serverServices";
import { preferenceEvent } from "../sync/uiPreferenceMerge";
import { createBrowserAppPreferences } from "./browserAppPreferences";
import { serverViewId } from "./environment";
import { studioPreferencesStorageKey } from "../studioPreferences";
import { viewServer } from "./registry";
export const useServerAttachmentSelections = () =>
  useServerServices()?.attachmentSelections ?? defaultAttachmentSelections;

const Context = createContext<ServerServices | null>(null);

/** A chat/project subtree receives the services of its own resource owner. */
export function ServerServicesProvider({
  services,
  children,
}: {
  services: ServerServices;
  children: ReactNode;
}) {
  return (
    <Context.Provider value={services}>
      <Fragment
        key={JSON.stringify([
          services.owner.id,
          services.owner.origin,
          services.owner.credentialId,
          services.owner.workspaceId,
        ])}
      >
        {children}
      </Fragment>
    </Context.Provider>
  );
}
export const useServerServices = () => useContext(Context);
export function useServerOwner() {
  const services = useServerServices();
  const [fallback] = useState(() =>
    services
      ? undefined
      : Object.freeze({ ...viewServer(serverViewId || "local") }),
  );
  const owner = services?.owner ?? fallback;
  if (!owner)
    throw new Error("The server service owner changed. Open this view again.");
  return owner;
}
export const useServerApi = () => useServerServices()?.api ?? defaultApi;
export function useServerSidebarServices() {
  const services = useServerServices();
  return useMemo(() => {
    if (!services) return localSidebarServices;
    // Keep sidebar writes on the same authenticated owner as this screen.
    const backend = serverSidebarBackend(services, () => true);
    return createSidebarServices(backend, [backend], () => backend.ownerId);
  }, [services]);
}
export const useServerSync = () => useServerServices()?.sync ?? defaultSync;
export const useServerResources = () =>
  useServerServices()?.resources ?? defaultResources;
export const useServerTranscripts = () =>
  useServerServices()?.transcripts ?? defaultTranscripts;
export const useServerTokenRates = () =>
  useServerServices()?.tokenRates ?? defaultTokenRates;

export const useServerOutbox = () =>
  useServerServices()?.outbox ?? defaultOutbox;

export function useServerDrafts() {
  const services = useServerServices();
  const useDrafts = services?.useDrafts ?? useSyncedDrafts;
  return useDrafts();
}

export const useServerActivityScope = () =>
  useServerServices()?.activity ?? defaultActivity;

export const useServerAccountLimits = () =>
  useServerServices()?.accountLimits ?? defaultAccountLimits;

export const useServerResourceReads = () =>
  useServerServices()?.watchResourceReads ?? watchResourceReads;

export const useServerFileDialogs = () =>
  useServerServices()?.fileDialogs ?? defaultFileDialogs;

export const useServerFileActions = () =>
  useServerServices()?.fileActions ?? defaultFileActions;

export const useServerStorage = () =>
  useServerServices()?.storage ?? serverLocalStorage;

export function useServerAppPreferences() {
  const services = useServerServices();
  return useMemo(
    () =>
      services?.appPreferences ??
      createBrowserAppPreferences(
        serverViewId || "local",
        serverLocalStorage,
        defaultApi.syncPost,
      ),
    [services],
  );
}

export const useServerChatCreations = () =>
  useServerServices()?.chatCreations ?? defaultChatCreations;

export const useServerLocalDrafts = () =>
  useServerServices()?.localDrafts ?? defaultLocalDrafts;

export const useServerStorageEventKey = () =>
  useServerServices()?.storageEventKey ?? serverStorageEventKey;

/** Expose logical changes from this owner's storage, not another server's keys. */
export function useServerPreferenceEvents() {
  const services = useServerServices();
  const storage = services?.storage ?? serverLocalStorage;
  const eventKey = services?.storageEventKey ?? serverStorageEventKey;
  return useMemo(
    () => ({
      subscribe(listener: (key: string | null) => void) {
        const seen = new Map<string, string | null>();
        for (let index = 0; index < storage.length; index++) {
          const key = storage.key(index);
          if (key !== null) seen.set(key, storage.getItem(key));
        }
        const receive = (event: StorageEvent) => {
          const key =
            event.key === studioPreferencesStorageKey
              ? event.key
              : eventKey(event);
          if (key !== undefined) listener(key);
        };
        const preference = (event: Event) => {
          if (
            !(event instanceof CustomEvent) ||
            typeof event.detail !== "string"
          )
            return;
          const key = event.detail;
          const raw = storage.getItem(key);
          if ((seen.get(key) ?? null) === raw) return;
          seen.set(key, raw);
          listener(key);
        };
        window.addEventListener("storage", receive);
        window.addEventListener(preferenceEvent, preference);
        return () => {
          window.removeEventListener("storage", receive);
          window.removeEventListener(preferenceEvent, preference);
        };
      },
    }),
    [storage, eventKey],
  );
}

export const useServerRecordings = () =>
  useServerServices()?.recordings ?? defaultRecordings;

export const useServerAudio = () =>
  useServerServices()?.audio ?? defaultServerAudio;

export const useServerUploads = () =>
  useServerServices()?.uploads ?? defaultUploads;

export const useServerSessionCosts = () =>
  useServerServices()?.sessionCosts ?? defaultSessionCosts;
