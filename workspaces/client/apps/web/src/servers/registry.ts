import { hasNativeClient } from "../native/client";
import {
  PAIRED_SERVERS_KEY,
  readPairedServerReferences,
  writePairedServerReferences,
} from "../browserPersistence";
import { isolatedServerView, serverParentOrigin } from "./environment";
import type { StudioServerIdentity } from "./serverIdentity.ts";
import {
  defaultServerAlias,
  DEFAULT_LOCAL_SERVER_ALIAS,
  validateServerAlias,
} from "./serverAliases";
export type StudioServer = StudioServerIdentity & {
  workspaceId?: string;
  alias?: string | undefined;
};
export function serverResourceScope(server?: Pick<StudioServer, "id">) {
  return !server || server.id === "current" ? "local" : server.id;
}
export const LOCAL_NAME_KEY = "studio-local-server-name-v1";
export const SERVER_REGISTRY_KEY = PAIRED_SERVERS_KEY;
export const SERVER_REGISTRY_EVENT = "studio-server-registry";
const metadata = new Map<string, StudioServer>();
let localDetails: { label?: string; alias?: string } = {};

export function setLocalServerDetails(details: {
  label?: string;
  alias?: string;
}) {
  if (
    (details.label === undefined || details.label === localDetails.label) &&
    (details.alias === undefined || details.alias === localDetails.alias)
  )
    return;
  localDetails = { ...localDetails, ...details };
  if (typeof window !== "undefined")
    window.dispatchEvent(new Event(SERVER_REGISTRY_EVENT));
}
export function serveOrigin(value: string) {
  const url = new URL(value.trim());
  if (
    url.protocol !== "https:" ||
    !url.hostname.endsWith(".ts.net") ||
    url.username ||
    url.password ||
    url.port === "0" ||
    url.pathname !== "/" ||
    url.search ||
    url.hash
  )
    throw new Error(
      "Enter the Tailscale Serve HTTPS address, such as https://computer.tailnet.ts.net.",
    );
  return url.origin;
}
export function readServers(storage?: Storage): StudioServer[] {
  const value = readPairedServerReferences(storage);
  const ids = new Set<string>();
  const aliases = new Set<string>([
    localDetails.alias || DEFAULT_LOCAL_SERVER_ALIAS,
  ]);
  for (const item of value) {
    const prior = metadata.get(item.id);
    if (
      prior?.origin === item.origin &&
      prior.credentialId === item.credentialId &&
      prior.alias
    ) {
      validateServerAlias(prior.alias, aliases);
      aliases.add(prior.alias);
    }
  }
  const servers = value.map((item) => {
    if (
      !/^[a-zA-Z0-9_-]{1,128}$/.test(item.id) ||
      item.id === "local" ||
      !item.credentialId ||
      ids.has(item.id)
    )
      throw new Error("The saved server list is invalid.");
    ids.add(item.id);
    const origin = serveOrigin(item.origin);
    const prior = metadata.get(item.id);
    const details =
      prior?.origin === origin && prior.credentialId === item.credentialId
        ? prior
        : undefined;
    const label = details?.label || new URL(origin).hostname;
    const alias =
      details?.alias || defaultServerAlias({ ...item, label }, aliases);
    aliases.add(alias);
    return {
      id: item.id,
      label,
      origin,
      credentialId: item.credentialId,
      alias,
      ...(details?.workspaceId ? { workspaceId: details.workspaceId } : {}),
    };
  });
  for (const id of metadata.keys()) if (!ids.has(id)) metadata.delete(id);
  return servers;
}
/** Keep each unchanged server object, and the list itself when nothing changed. A reread
 * of the same saved list must not look like a new server to open dialogs and connections. */
export function shareServers(
  previous: readonly StudioServer[],
  next: StudioServer[],
): StudioServer[] {
  const prior = new Map(previous.map((server) => [server.id, server]));
  const shared = next.map((server) => {
    const old = prior.get(server.id);
    return old && JSON.stringify(old) === JSON.stringify(server) ? old : server;
  });
  return shared.length === previous.length &&
    shared.every((server, index) => server === previous[index])
    ? (previous as StudioServer[])
    : shared;
}
export function writeServers(servers: StudioServer[], storage?: Storage) {
  const aliases = new Set([localDetails.alias || DEFAULT_LOCAL_SERVER_ALIAS]);
  for (const server of servers) {
    if (server.alias) {
      validateServerAlias(server.alias, aliases);
      aliases.add(server.alias);
    }
  }
  const normalized = servers.map((server) => {
    const alias = server.alias || defaultServerAlias(server, aliases);
    aliases.add(alias);
    return { ...server, alias };
  });
  writePairedServerReferences(
    normalized.map((server) => {
      if (!server.credentialId)
        throw new Error("The saved server list is invalid.");
      return {
        id: server.id,
        origin: server.origin,
        credentialId: server.credentialId,
      };
    }),
    storage,
  );
  metadata.clear();
  for (const server of normalized) metadata.set(server.id, server);
  if (typeof window !== "undefined")
    window.dispatchEvent(
      new CustomEvent(SERVER_REGISTRY_EVENT, { detail: normalized }),
    );
}
export function localServer(): StudioServer {
  return {
    id: "local",
    label: localDetails.label || "This computer",
    origin: hasNativeClient() ? "http://localhost" : location.origin,
    alias: localDetails.alias || DEFAULT_LOCAL_SERVER_ALIAS,
  };
}
export function readServerAliases(): Record<string, string> {
  try {
    return Object.fromEntries(
      [localServer(), ...readServers()].map((server) => [
        server.id,
        server.alias || DEFAULT_LOCAL_SERVER_ALIAS,
      ]),
    );
  } catch {
    return { local: localServer().alias || DEFAULT_LOCAL_SERVER_ALIAS };
  }
}
export function viewServer(id: string): StudioServer {
  if (isolatedServerView) {
    const params = new URLSearchParams(location.search);
    if (params.get("studio-server") !== id)
      throw new Error("The server view has another owner.");
    const origin = params.get("studio-origin") || "";
    return {
      id,
      label: id,
      origin:
        id === "local"
          ? location.protocol === "studio-view:"
            ? "http://localhost"
            : serverParentOrigin
          : serveOrigin(origin),
      ...(params.get("studio-credential")
        ? { credentialId: params.get("studio-credential")! }
        : {}),
    };
  }
  if (id === "local") return localServer();
  const server = readServers().find((row) => row.id === id);
  if (!server)
    throw new Error("This server was removed. Select a paired server.");
  return server;
}
