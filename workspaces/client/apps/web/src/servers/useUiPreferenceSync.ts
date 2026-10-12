import { orderPreferenceServers } from "./preferenceServerOrder";
import { legacyPreferenceSeed } from "./legacyPreferenceSeed";
import { useEffect, useRef } from "react";
import { fetchApiRequest } from "../api";
import { connectUiPreferences } from "../sync/uiPreferenceConnection";
import { createPreferenceSyncConnectionManager } from "./preferenceSyncConnectionManager";
import {
  capturePreferenceWrite,
  preferenceStorageForServer,
  readPreferenceFields,
  receivePreferenceFields,
  watchPreferenceWrites,
} from "../sync/uiPreferenceStore";
import { preferenceEvent } from "../sync/uiPreferenceMerge";
import { serverCredentialAdapter } from "./transport";
import {
  defaultServerAlias,
  DEFAULT_LOCAL_SERVER_ALIAS,
  LOCAL_ALIAS_KEY,
} from "./serverAliases";
import {
  readServers,
  SERVER_REGISTRY_EVENT,
  SERVER_REGISTRY_KEY,
  LOCAL_NAME_KEY,
  type StudioServer,
} from "./registry";

export function useUiPreferenceSync(
  servers: StudioServer[],
  lastSeenByServer: Record<string, number | undefined> = {},
) {
  const ownership = JSON.stringify(
    servers.map(({ id, origin, credentialId }) => [id, origin, credentialId]),
  );
  const usableSnapshot = JSON.stringify(
    servers.map(({ id }) => [id, lastSeenByServer[id]]),
  );
  const serverSnapshot = JSON.stringify(servers);
  const connections = useRef<ReturnType<
    typeof createPreferenceSyncConnectionManager<StudioServer>
  > | null>(null);
  useEffect(() => {
    let applying = false;
    const identities = new Map<string, string>();
    const snapshots = new Map<string, { alias?: string; label: string }>();
    let previousOrder: string[] | undefined;
    for (const server of servers) {
      const identity =
        server.workspaceId ||
        localStorage.getItem(`studio-server-workspace-id:${server.id}`);
      if (identity) identities.set(server.id, identity);
      snapshots.set(server.id, { alias: server.alias, label: server.label });
    }
    const keyFor = (kind: string, identity: string) =>
      `server-${kind}:${identity}`;
    const seed = () => {
      const fields = legacyPreferenceSeed(
        servers,
        identities,
        readPreferenceFields("user"),
        localStorage.getItem(LOCAL_NAME_KEY),
      );
      if (Object.keys(fields).length) receivePreferenceFields("user", fields);
    };
    const apply = () => {
      if (applying) return;
      applying = true;
      try {
        const fields = readPreferenceFields("user");
        const value = (key: string) => fields[JSON.stringify([key])]?.value;
        const order = value("server-order");
        const rank = (server: StudioServer) => {
          const index = Array.isArray(order)
            ? order.indexOf(identities.get(server.id) || "")
            : -1;
          return index < 0 ? Number.MAX_SAFE_INTEGER : index;
        };
        const registered = readServers();
        const used = new Set<string>();
        const local = servers.find((server) => server.id === "local");
        if (local) {
          const identity = identities.get("local");
          const alias = identity && value(keyFor("alias", identity));
          const current =
            typeof alias === "string"
              ? alias
              : localStorage.getItem(LOCAL_ALIAS_KEY) ||
                local.alias ||
                DEFAULT_LOCAL_SERVER_ALIAS;
          localStorage.setItem(LOCAL_ALIAS_KEY, current);
          used.add(current);
          snapshots.set("local", { alias: current, label: local.label });
        }
        const paired = registered
          .map((server) => {
            const identity = identities.get(server.id);
            const alias = identity && value(keyFor("alias", identity));
            const label = identity && value(keyFor("label", identity));
            const next = {
              ...server,
              ...(typeof label === "string" ? { label } : {}),
            };
            const desired = typeof alias === "string" ? alias : server.alias;
            next.alias =
              desired && !used.has(desired)
                ? desired
                : defaultServerAlias(next, used);
            used.add(next.alias);
            snapshots.set(server.id, { alias: next.alias, label: next.label });
            return next;
          })
          .sort((a, b) => rank(a) - rank(b));
        const serialized = JSON.stringify(paired);
        if (localStorage.getItem(SERVER_REGISTRY_KEY) !== serialized)
          localStorage.setItem(SERVER_REGISTRY_KEY, serialized);
        previousOrder = orderPreferenceServers([
          ...(local ? [local] : []),
          ...paired,
        ]).map((row) => identities.get(row.id) || row.id);
        window.dispatchEvent(new Event(SERVER_REGISTRY_EVENT));
      } finally {
        applying = false;
      }
    };
    const capture = () => {
      if (applying) return;
      const paired = readServers();
      const current = [...paired];
      const localIndex = servers.findIndex((server) => server.id === "local");
      if (localIndex >= 0)
        current.splice(localIndex, 0, {
          ...servers[localIndex],
          alias:
            localStorage.getItem(LOCAL_ALIAS_KEY) || DEFAULT_LOCAL_SERVER_ALIAS,
        });
      for (const server of current) {
        const identity = identities.get(server.id);
        const prior = snapshots.get(server.id);
        if (!identity || !prior) continue;
        for (const kind of ["alias", "label"] as const) {
          if (server[kind] !== prior[kind])
            capturePreferenceWrite(
              keyFor(kind, identity),
              JSON.stringify(server[kind]),
              JSON.stringify(prior[kind]),
            );
        }
        snapshots.set(server.id, { alias: server.alias, label: server.label });
      }
      const order = current.map(
        (server) => identities.get(server.id) || server.id,
      );
      if (
        previousOrder &&
        JSON.stringify(previousOrder) !== JSON.stringify(order)
      )
        capturePreferenceWrite(
          "server-order",
          JSON.stringify(order),
          JSON.stringify(previousOrder),
        );
      previousOrder = order;
    };
    seed();
    const manager = createPreferenceSyncConnectionManager<StudioServer>(
      (server, onSchemaMismatch) =>
        connectUiPreferences(
          server.origin,
          server.id === "local"
            ? fetchApiRequest
            : (request) => serverCredentialAdapter().fetch(server, request),
          preferenceStorageForServer(server.id),
          (identity) => {
            if (identities.get(server.id) === identity) return;
            identities.set(server.id, identity);
            localStorage.setItem(
              `studio-server-workspace-id:${server.id}`,
              identity,
            );
            seed();
            apply();
          },
          onSchemaMismatch,
        ),
    );
    connections.current = manager;
    manager.updateServers(servers, lastSeenByServer);
    const stopWrites = watchPreferenceWrites(apply);
    window.addEventListener(SERVER_REGISTRY_EVENT, capture);
    const stored = (event: StorageEvent) => {
      if (event.key === SERVER_REGISTRY_KEY || event.key === LOCAL_ALIAS_KEY)
        capture();
    };
    window.addEventListener("storage", stored);
    const visual = (event: Event) => {
      if (
        event instanceof CustomEvent &&
        typeof event.detail === "string" &&
        event.detail.startsWith("server-")
      )
        apply();
    };
    window.addEventListener(preferenceEvent, visual);
    return () => {
      manager.stop();
      if (connections.current === manager) connections.current = null;
      stopWrites();
      window.removeEventListener(SERVER_REGISTRY_EVENT, capture);
      window.removeEventListener("storage", stored);
      window.removeEventListener(preferenceEvent, visual);
    };
  }, [ownership]);
  useEffect(() => {
    connections.current?.updateServers(servers, lastSeenByServer);
    connections.current?.updateLastSeen(lastSeenByServer);
  }, [ownership, serverSnapshot, usableSnapshot]);
}
