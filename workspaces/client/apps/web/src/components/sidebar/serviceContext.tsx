import { createContext, useContext, useEffect, useState } from "react";
import type * as resources from "../../sync/subscriptions/events";
import type { post, saved, save } from "../../api";
import type { writePreferenceEdit } from "../../sync/uiPreferenceStore";
import type { PreferenceFields } from "../../sync/uiPreferenceMerge";
import type { Project } from "../../types";
import type { LayrReadinessEntry } from "./LayrReadiness";
import type { LayrSyncEntry } from "./MacSync";
import type { VaultEntry } from "./Vault";

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
  resources?: Pick<
    typeof resources,
    "watchResourceChanges" | "watchResourceConnection"
  >;
  saved: typeof saved;
  save: typeof save;
  storage: Pick<Storage, "getItem" | "setItem" | "removeItem">;
  editPreference: typeof writePreferenceEdit;
  viewPreference?: typeof writePreferenceEdit;
  subscribePreference: (key: string, update: () => void) => () => void;
  /** This window's server-scope preference fields with their edit times. */
  preferenceFields?: () => PreferenceFields;
  /** Merge fields by edit time before an edit, so the edit is newer than every shown value. */
  adoptPreferenceFields?: (fields: PreferenceFields) => void;
};

export type SidebarServices = {
  cache: SidebarBackend;
  owner: (target: SidebarTarget) => SidebarBackend;
  byOwner: (ownerId: string) => SidebarBackend;
  serverForChat: (id: string) => string;
  available?: (target: SidebarTarget) => boolean;
  /** Why actions on the target are disabled, or null when they are available. */
  unavailable?: (target: SidebarTarget) => string | null;
  project?: (path: string, target: SidebarTarget) => Project;
  displayPath?: (path: string, target?: SidebarTarget) => string;
  chatPath?: (id: string) => string;
  alias?: (target: SidebarTarget) => string | undefined;
  /** Each server's own report for a combined project row. */
  layrReadiness?: (path: string) => LayrReadinessEntry[];
  /** Each server's Mac folder sync of a combined project row, with its owner for actions. */
  layrSync?: (path: string) => LayrSyncEntry[];
  /** Each server's report of the project's vault, with its owner for actions. */
  vault?: (path: string) => VaultEntry[];
};

export function sidebarIdentity(ownerId: string, id: string): string {
  return JSON.stringify([ownerId, id]);
}

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

const SidebarBackendContext = createContext<SidebarBackend | null>(null);
export const SidebarBackendProvider = SidebarBackendContext.Provider;
export const useOptionalSidebarBackend = () =>
  useContext(SidebarBackendContext);
export function useSidebarBackend() {
  const backend = useOptionalSidebarBackend();
  if (!backend) throw new Error("The sidebar requires its own service owner.");
  return backend;
}

export function useSidebarVisualState<T>(key: string, fallback: T) {
  return useSidebarVisualStateFor(useSidebarBackend(), key, fallback);
}
export function useSidebarVisualStateFor<T>(
  backend: SidebarBackend,
  key: string,
  fallback: T,
) {
  const [value, setValue] = useState<T>(() => backend.saved(key, fallback));
  useEffect(() => {
    const update = () => setValue(backend.saved(key, fallback));
    update();
    return backend.subscribePreference(key, update);
  }, [backend, key]);
  return [value, setValue] as const;
}
