import { SignedEventSource } from "../../servers/eventSource";
import { routesFor } from "../../servers/routes";
import { hasNativeClient } from "../../native/client";
import { NativeEventSource } from "../../native/stream";
import type { createApiScope } from "../../api";
import type { createSyncClient } from "../client";
import {
  createResourceEvents,
  type ResourceEventsOptions,
} from "../resourceEvents";
import { MemoryTransferStorage } from "../entityTransfer";
import { createSubscriptions } from "./module";
import { createDataObserver } from "./data";
import { createEntityDataObserver } from "./entityData";

type ServerSubscriptionOptions = {
  api: ResourceEventsOptions["api"] &
    Pick<ReturnType<typeof createApiScope>, "fetchApiRequest">;
  tokenRates: ResourceEventsOptions["tokenRates"];
  sync: () => ReturnType<typeof createSyncClient>;
  owner: { id: string; origin: string };
  /** The native core owns this server's stream and projections: no browser storage, locks or channels. */
  nativeCore?: () => boolean;
};

/** The server service supplies ownership and cache access, never stream details. */
export function createServerSubscriptions(options: ServerSubscriptionOptions) {
  const { api, tokenRates, sync, owner, nativeCore } = options;
  const transport = createResourceEvents({
    api,
    tokenRates,
    sync: {
      syncDatabase: () => sync().syncDatabase(),
      persistResourceEntityChanges: (...args) =>
        sync().persistResourceEntityChanges(...args),
      readSubscriptionCursor: (ref) => sync().readSubscriptionCursor(ref),
      persistStagedEntityTransfer: (...args) =>
        sync().persistStagedEntityTransfer(...args),
    },
    // The local server's requests use the native client there (serverServices.ts).
    transferStorage: () =>
      nativeCore?.() ? new MemoryTransferStorage() : undefined,
    ...(nativeCore?.() ? { independent: true } : {}),
    // Each connection of a remote server reports the routes it answers on.
    ...(owner.id === "local"
      ? {}
      : {
          onRoutes: (routes: readonly string[]) =>
            routesFor(owner).learn(routes),
        }),
    openEvents: (url) =>
      owner.id === "local" && hasNativeClient()
        ? new NativeEventSource(url)
        : new SignedEventSource(
            new URL(url, owner.origin).href,
            api.fetchApiRequest,
          ),
    storageName: (name) =>
      owner.id === "local" ? name : `${name}:server:${owner.id}`,
  });
  // A move to a faster route replaces the stream.
  const stopRoutes =
    owner.id === "local"
      ? undefined
      : routesFor(owner).onChange(() => transport.replaceStream());
  return createSubscriptions(
    {
      ...transport,
      dispose() {
        stopRoutes?.();
        transport.dispose();
      },
    },
    createDataObserver(
      sync,
      transport,
      createEntityDataObserver(() => sync().syncDatabase()),
      {
        processes: (ref) =>
          api.get("/api/processes", { query: { agent: ref.agentId } }),
        complaint: (ref) =>
          api.get("/api/complaint", { query: { id: ref.complaintId } }),
        room: (ref) =>
          ref.legacy
            ? api.get("/api/messages", { query: { room: ref.roomId } })
            : api.get("/api/agent-chat", { query: { room: ref.roomId } }),
        "terminal-output": (ref) =>
          api.get("/api/terminals/output", { query: { id: ref.terminalId } }),
      },
    ),
  );
}
