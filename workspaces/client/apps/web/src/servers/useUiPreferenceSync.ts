import { orderPreferenceServers } from "./preferenceServerOrder";
import { useEffect } from "react";
import { connectUiPreferences } from "../sync/uiPreferenceConnection";
import {
  capturePreferenceWrite,
  preferenceStorageForServer,
  readPreferenceFields,
  receivePreferenceFields,
  watchPreferenceWrites,
} from "../sync/uiPreferenceStore";
import { preferenceEvent } from "../sync/uiPreferenceMerge";
import { serverCredentialAdapter } from "./transport";
import { defaultServerAlias, LOCAL_ALIAS_KEY } from "./serverAliases";
import {
  readServers,
  SERVER_REGISTRY_EVENT,
  SERVER_REGISTRY_KEY,
  type StudioServer,
} from "./registry";

export function useUiPreferenceSync(servers: StudioServer[]) {
  const ownership = JSON.stringify(
    servers.map(({ id, origin, credentialId }) => [id, origin, credentialId]),
  );
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
    const seed = (server: StudioServer, identity: string) => {
      const fields = readPreferenceFields("user");
      for (const [kind, value] of [
        ["alias", server.alias],
        ["label", server.label],
      ] as const) {
        const name = JSON.stringify([keyFor(kind, identity)]);
        if (value && !fields[name])
          fields[name] = { value, timestamp: 0, writer: "legacy" };
      }
      const orderKey = JSON.stringify(["server-order"]);
      if (!fields[orderKey])
        fields[orderKey] = {
          value: servers
            .map((row) => identities.get(row.id))
            .filter((id): id is string => !!id),
          timestamp: 0,
          writer: "legacy",
        };
      receivePreferenceFields("user", fields);
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
              : localStorage.getItem(LOCAL_ALIAS_KEY) || local.alias || "MAC";
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
          alias: localStorage.getItem(LOCAL_ALIAS_KEY) || "MAC",
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
    for (const server of servers) {
      const identity = identities.get(server.id);
      if (identity) seed(server, identity);
    }
    const stops = servers.map((server) =>
      connectUiPreferences(
        server.origin,
        (request) =>
          server.id === "local"
            ? fetch(request)
            : serverCredentialAdapter().fetch(server, request),
        preferenceStorageForServer(server.id),
        (identity) => {
          if (identities.get(server.id) === identity) return;
          identities.set(server.id, identity);
          localStorage.setItem(
            `studio-server-workspace-id:${server.id}`,
            identity,
          );
          seed(server, identity);
          apply();
        },
      ),
    );
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
      stops.forEach((stop) => stop());
      stopWrites();
      window.removeEventListener(SERVER_REGISTRY_EVENT, capture);
      window.removeEventListener("storage", stored);
      window.removeEventListener(preferenceEvent, visual);
    };
  }, [ownership]);
}
