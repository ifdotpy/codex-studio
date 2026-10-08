import { isolatedServerView, serverParentOrigin } from "./environment";
export type StudioServer = {
  id: string;
  label: string;
  origin: string;
  credentialId?: string;
  workspaceId?: string;
};
export const SERVER_REGISTRY_KEY = "studio-paired-servers-v1";
export const SERVER_REGISTRY_EVENT = "studio-server-registry";
export function serveOrigin(value: string) {
  const url = new URL(value.trim());
  if (
    url.protocol !== "https:" ||
    !url.hostname.endsWith(".ts.net") ||
    url.username ||
    url.password ||
    url.port ||
    url.pathname !== "/" ||
    url.search ||
    url.hash
  )
    throw new Error(
      "Enter the Tailscale Serve HTTPS address, such as https://computer.tailnet.ts.net.",
    );
  return url.origin;
}
export function readServers(storage = globalThis.localStorage): StudioServer[] {
  const value: unknown = JSON.parse(
    storage.getItem(SERVER_REGISTRY_KEY) || "[]",
  );
  if (!Array.isArray(value))
    throw new Error("The saved server list is invalid.");
  const ids = new Set<string>();
  return value.map((item) => {
    if (
      !item ||
      typeof item.id !== "string" ||
      !/^[a-zA-Z0-9_-]{1,128}$/.test(item.id) ||
      item.id === "local" ||
      typeof item.label !== "string" ||
      !item.label.trim() ||
      item.label.length > 128 ||
      typeof item.credentialId !== "string" ||
      !item.credentialId ||
      ids.has(item.id)
    )
      throw new Error("The saved server list is invalid.");
    ids.add(item.id);
    return {
      id: item.id,
      label: item.label,
      origin: serveOrigin(item.origin),
      credentialId: item.credentialId,
      ...(typeof item.workspaceId === "string"
        ? { workspaceId: item.workspaceId }
        : {}),
    };
  });
}
export function writeServers(
  servers: StudioServer[],
  storage = globalThis.localStorage,
) {
  storage.setItem(SERVER_REGISTRY_KEY, JSON.stringify(servers));
  if (typeof window !== "undefined")
    window.dispatchEvent(new Event(SERVER_REGISTRY_EVENT));
}
export function localServer(): StudioServer {
  return { id: "local", label: "This computer", origin: location.origin };
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
      origin: id === "local" ? serverParentOrigin : serveOrigin(origin),
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
