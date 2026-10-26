import { createContext, useContext, Fragment, type ReactNode } from "react";
import type { ServerServices } from "./serverServices";
export type NativeServicePorts = ServerServices;

const Context = createContext<NativeServicePorts | null>(null);

/** A native screen cannot acquire services from another selected server. */
export function NativeServicesProvider({
  services,
  children,
}: {
  services: NativeServicePorts;
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
export const ServerServicesProvider = NativeServicesProvider;

export function useServerServices() {
  const services = useContext(Context);
  if (!services)
    throw new Error("The native screen requires its own server services.");
  return services;
}
export const useServerOwner = () => useServerServices().owner;
export const useServerApi = () => useServerServices().api;
export const useServerSync = () => useServerServices().sync;
export const useServerResources = () => useServerServices().resources;
export const useServerSidebarServices = () =>
  useServerServices().sidebarServices;
export const useServerTranscripts = () => useServerServices().transcripts;
export const useServerTokenRates = () => useServerServices().tokenRates;
export const useServerOutbox = () => useServerServices().outbox;
export const useServerDrafts = () => useServerServices().useDrafts();
export const useServerActivityScope = () => useServerServices().activity;
export const useServerAccountLimits = () => useServerServices().accountLimits;
export const useServerResourceReads = () =>
  useServerServices().watchResourceReads;
export const useServerFileDialogs = () => useServerServices().fileDialogs;
export const useServerFileActions = () => useServerServices().fileActions;
export const useServerStorage = () => useServerServices().storage;
export const useServerChatCreations = () => useServerServices().chatCreations;
export const useServerLocalDrafts = () => useServerServices().localDrafts;
export const useServerStorageEventKey = () =>
  useServerServices().storageEventKey;
export const useServerPreferenceEvents = () =>
  useServerServices().preferenceEvents;
export const useServerAppPreferences = () => useServerServices().appPreferences;
export const useServerRecordings = () => useServerServices().recordings;
export const useServerAudio = () => useServerServices().audio;
export const useServerUploads = () => useServerServices().uploads;
export const useServerAttachmentSelections = () =>
  useServerServices().attachmentSelections;
export const useServerSessionCosts = () => useServerServices().sessionCosts;
