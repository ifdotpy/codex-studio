import * as defaultSessionCosts from "../usage/sessionCostCache";
import * as defaultUploads from "../sync/uploads";
import { defaultServerAudio } from "../native/serverAudio";
import { draftsForServer } from "../native/draftSource";
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
import * as defaultOutbox from "../sync/send";
import {
  createContext,
  useContext,
  useMemo,
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
import { coreStores, type ServerServices } from "./serverServices";
import { hasNativeClient, pageCore } from "../native/client";

const Context = createContext<ServerServices | null>(null);

// A server view page has one owner. Outside a provider, the core of that
// server keeps its queue, files, recordings and drafts.
let pageStores: ReturnType<typeof coreStores> | undefined;
function pageCoreStores() {
  if (!hasNativeClient()) return undefined;
  return (pageStores ??= coreStores(pageCore(), () =>
    defaultSync.outboxDatabase(),
  ));
}

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
        ])}
      >
        {children}
      </Fragment>
    </Context.Provider>
  );
}
export const useServerServices = () => useContext(Context);
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

/** The client core of the server of this subtree, or undefined without one. */
export const useServerCore = () =>
  useServerServices()?.core ?? (hasNativeClient() ? pageCore() : undefined);

export const useServerOutbox = () =>
  useServerServices()?.outbox ?? pageCoreStores()?.outbox ?? defaultOutbox;

export function useServerDrafts() {
  const services = useServerServices();
  const useDrafts =
    services?.useDrafts ??
    pageCoreStores()?.useDrafts ??
    draftsForServer(undefined);
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

export const useServerChatCreations = () =>
  useServerServices()?.chatCreations ?? defaultChatCreations;

export const useServerLocalDrafts = () =>
  useServerServices()?.localDrafts ?? defaultLocalDrafts;

export const useServerStorageEventKey = () =>
  useServerServices()?.storageEventKey ?? serverStorageEventKey;

export const useServerRecordings = () =>
  useServerServices()?.recordings ??
  pageCoreStores()?.recordings ??
  defaultRecordings;

export const useServerAudio = () =>
  useServerServices()?.audio ?? defaultServerAudio;

export const useServerUploads = () =>
  useServerServices()?.uploads ?? pageCoreStores()?.uploads ?? defaultUploads;

export const useServerSessionCosts = () =>
  useServerServices()?.sessionCosts ?? defaultSessionCosts;
