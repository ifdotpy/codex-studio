import { createContext, useContext, useEffect, useState } from "react";
import { post, saved, save } from "../../api";
import { serverViewId } from "../../servers/environment";
import { serverLocalStorage } from "../../servers/storage";
import { preferenceEvent } from "../../sync/uiPreferenceMerge";
import { writePreferenceEdit } from "../../sync/uiPreferenceStore";
import type { Project } from "../../types";

export type SidebarTarget =
  | { kind: "chat"; id: string }
  | {
      kind: "project";
      path: string;
      folder?: string;
      team?: string;
      chat?: string;
    }
  | { kind: "order"; group: string };

/** One immutable server connection, including its original saved-state keys. */
export type SidebarBackend = {
  ownerId: string;
  post: typeof post;
  saved: typeof saved;
  save: typeof save;
  storage: Pick<Storage, "getItem" | "setItem" | "removeItem">;
  editPreference: typeof writePreferenceEdit;
  subscribePreference: (key: string, update: () => void) => () => void;
};

export type SidebarServices = {
  cache: SidebarBackend;
  owner: (target: SidebarTarget) => SidebarBackend;
  byOwner: (ownerId: string) => SidebarBackend;
  serverForChat: (id: string) => string;
  available?: (target: SidebarTarget) => boolean;
  project?: (path: string, target: SidebarTarget) => Project;
  displayPath?: (path: string, target?: SidebarTarget) => string;
  chatPath?: (id: string) => string;
  alias?: (target: SidebarTarget) => string | undefined;
};

export function sidebarIdentity(ownerId: string, id: string): string {
  return JSON.stringify([ownerId, id]);
}

export const localSidebarBackend: SidebarBackend = {
  ownerId: serverViewId || "local",
  post,
  saved,
  save,
  storage: serverLocalStorage,
  editPreference: writePreferenceEdit,
  subscribePreference(key, update) {
    const receive = (event: Event) => {
      if (event instanceof CustomEvent && event.detail !== key) return;
      if (event instanceof StorageEvent && event.key !== key) return;
      update();
    };
    window.addEventListener(preferenceEvent, receive);
    window.addEventListener("storage", receive);
    return () => {
      window.removeEventListener(preferenceEvent, receive);
      window.removeEventListener("storage", receive);
    };
  },
};

/** Render identities resolve here; backend adapters retain original wire IDs. */
export function createSidebarServices(
  cache: SidebarBackend,
  backends: readonly SidebarBackend[],
  ownerIdFor: (target: SidebarTarget) => string,
): SidebarServices {
  const owners = new Map<string, SidebarBackend>();
  for (const backend of backends) {
    if (owners.has(backend.ownerId))
      throw new Error("Duplicate sidebar server identity.");
    owners.set(backend.ownerId, backend);
  }
  const byOwner = (ownerId: string) => {
    const backend = owners.get(ownerId);
    if (!backend) throw new Error("The sidebar server is unavailable.");
    return backend;
  };
  return {
    cache,
    byOwner,
    owner: (target) => byOwner(ownerIdFor(target)),
    serverForChat: (id) => byOwner(ownerIdFor({ kind: "chat", id })).ownerId,
  };
}

export const localSidebarServices = createSidebarServices(
  localSidebarBackend,
  [localSidebarBackend],
  () => localSidebarBackend.ownerId,
);

const SidebarBackendContext = createContext(localSidebarBackend);
export const SidebarBackendProvider = SidebarBackendContext.Provider;
export const useSidebarBackend = () => useContext(SidebarBackendContext);

export function useSidebarVisualState<T>(key: string, fallback: T) {
  const backend = useSidebarBackend();
  const [value, setValue] = useState<T>(() => backend.saved(key, fallback));
  useEffect(() => {
    const update = () => setValue(backend.saved(key, fallback));
    update();
    return backend.subscribePreference(key, update);
  }, [backend, key]);
  return [value, setValue] as const;
}
