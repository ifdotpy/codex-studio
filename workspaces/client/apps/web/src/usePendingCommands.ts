import { useSyncExternalStore } from "react";
import { useServerApi } from "./servers/ServerServicesContext";
import type { PendingEntry } from "./pendingCommands";

/** The pending commands of this server, in the order they began. */
export function usePendingCommands(): readonly PendingEntry[] {
  const { pending } = useServerApi();
  return useSyncExternalStore(pending.subscribe, pending.list, pending.list);
}
