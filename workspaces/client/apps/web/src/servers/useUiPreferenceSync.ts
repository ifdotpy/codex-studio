import { orderPreferenceServers } from "./preferenceServerOrder";
import { useEffect } from "react";
import {
  capturePreferenceWrite,
  readPreferenceFields,
  watchPreferenceWrites,
} from "../sync/uiPreferenceStore";
import { preferenceEvent } from "../sync/uiPreferenceMerge";
import {
  defaultServerAlias,
  DEFAULT_LOCAL_SERVER_ALIAS,
} from "./serverAliases";
import {
  readServers,
  localServer,
  setLocalServerDetails,
  writeServers,
  SERVER_REGISTRY_EVENT,
  type StudioServer,
} from "./registry";

/** Applies shell registry preferences; each mounted App owns its projection feed. */
export function useUiPreferenceSync(servers: StudioServer[]) {
  const ownership = JSON.stringify(
    servers.map(({ id, origin, credentialId, workspaceId }) => [
      id,
      origin,
      credentialId,
      workspaceId,
    ]),
  );
  useEffect(() => {
    let applying = false;
    const identities = new Map<string, string>();
    const snapshots = new Map<
      string,
      { alias?: string | undefined; label: string }
    >();
    let previousOrder: string[] | undefined;
    for (const server of servers) {
      const identity = server.workspaceId;
      if (identity) identities.set(server.id, identity);
      snapshots.set(server.id, { alias: server.alias, label: server.label });
    }
    const keyFor = (kind: string, identity: string) =>
      `server-${kind}:${identity}`;
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
              : local.alias || DEFAULT_LOCAL_SERVER_ALIAS;
          setLocalServerDetails({ alias: current });
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
        writeServers(paired);
        previousOrder = orderPreferenceServers([
          ...(local ? [local] : []),
          ...paired,
        ]).map((row) => identities.get(row.id) || row.id);
        window.dispatchEvent(new Event(SERVER_REGISTRY_EVENT));
      } finally {
        applying = false;
      }
    };
    const capture = (event: Event) => {
      if (applying) return;
      const reordered =
        event instanceof CustomEvent && Array.isArray(event.detail);
      const paired: StudioServer[] = reordered ? event.detail : readServers();
      const current = [...paired];
      const local = servers.find((server) => server.id === "local");
      if (local)
        current.splice(servers.indexOf(local), 0, {
          ...local,
          alias: localServer().alias || DEFAULT_LOCAL_SERVER_ALIAS,
        });
      if (!reordered)
        current.splice(0, current.length, ...orderPreferenceServers(current));
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
    apply();
    const stopWrites = watchPreferenceWrites(apply);
    window.addEventListener(SERVER_REGISTRY_EVENT, capture);
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
      stopWrites();
      window.removeEventListener(SERVER_REGISTRY_EVENT, capture);
      window.removeEventListener(preferenceEvent, visual);
    };
  }, [ownership]);
}
