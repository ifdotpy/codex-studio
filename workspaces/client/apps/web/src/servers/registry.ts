import { hasNativeClient } from "../native/client";
import { isolatedServerView, serverParentOrigin } from "./environment";
import {
  DEFAULT_LOCAL_SERVER_ALIAS,
  LOCAL_ALIAS_KEY,
} from "./serverAliasModel";
import {
  LOCAL_NAME_KEY,
  SERVER_REGISTRY_KEY,
  SERVER_REGISTRY_EVENT,
  readRegistryServers,
  normalizeServerAliases,
  localServerMetadata,
  serveOrigin,
  type StudioServer,
} from "./registryModel";
export * from "./registryModel";
export function readServers(storage = globalThis.localStorage): StudioServer[] {
  return readRegistryServers(storage);
}
export { shareServers } from "./registrySharing";
export function writeServers(
  servers: StudioServer[],
  storage = globalThis.localStorage,
) {
  const normalized = normalizeServerAliases(
    servers,
    storage.getItem(LOCAL_ALIAS_KEY) || DEFAULT_LOCAL_SERVER_ALIAS,
  );
  storage.setItem(SERVER_REGISTRY_KEY, JSON.stringify(normalized));
  if (typeof window !== "undefined")
    window.dispatchEvent(new Event(SERVER_REGISTRY_EVENT));
}
export function localServer(): StudioServer {
  return localServerMetadata({
    label: localStorage.getItem(LOCAL_NAME_KEY) || "This computer",
    origin: hasNativeClient() ? "http://localhost" : location.origin,
    alias: localStorage.getItem(LOCAL_ALIAS_KEY) || DEFAULT_LOCAL_SERVER_ALIAS,
  });
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
