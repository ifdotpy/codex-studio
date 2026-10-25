import { SignedEventSource } from "../../servers/eventSource";
import type { createApiScope } from "../../api";
import type { createSyncClient } from "../client";
import {
  createResourceEvents,
  type ResourceEventsOptions,
} from "../resourceEvents";
import { createSubscriptions } from "./module";
import { createDataObserver } from "./data";
import { createEntityDataObserver } from "./entityData";

type ServerSubscriptionOptions = {
  api: ResourceEventsOptions["api"] &
    Pick<ReturnType<typeof createApiScope>, "fetchApiRequest">;
  tokenRates: ResourceEventsOptions["tokenRates"];
  sync: () => ReturnType<typeof createSyncClient>;
  owner: { id: string; origin: string };
};

/** The server service supplies ownership and cache access, never stream details. */
export function createServerSubscriptions(options: ServerSubscriptionOptions) {
  const { api, tokenRates, sync, owner } = options;
  const transport = createResourceEvents({
    api,
    tokenRates,
    sync: {
      syncDatabase: () => sync().syncDatabase(),
      persistResourceEntityChanges: (...args) =>
        sync().persistResourceEntityChanges(...args),
      readSubscriptionCursor: (ref) => sync().readSubscriptionCursor(ref),
    },
    openEvents: (url) =>
      new SignedEventSource(
        new URL(url, owner.origin).href,
        api.fetchApiRequest,
      ),
    storageName: (name) =>
      owner.id === "local" ? name : `${name}:server:${owner.id}`,
  });
  return createSubscriptions(
    transport,
    createDataObserver(
      sync,
      transport,
      createEntityDataObserver(() => sync().syncDatabase()),
      async (ref) => {
        switch (ref.kind) {
          case "processes":
            return api.get("/api/processes", { query: { agent: ref.agentId } });
          case "work":
            return api.get("/api/work", { query: { agent: ref.agentId } });
          case "complaint":
            return api.get("/api/complaint", {
              query: { id: ref.complaintId },
            });
          case "room":
            return ref.legacy
              ? api.get("/api/messages", { query: { room: ref.roomId } })
              : api.get("/api/agent-chat", { query: { room: ref.roomId } });
          case "terminal-output":
            return api.get("/api/terminals/output", {
              query: { id: ref.terminalId },
            });
          default:
            throw new Error("Unsupported view recovery");
        }
      },
    ),
  );
}
