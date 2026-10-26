import {
  defaultServerAlias,
  DEFAULT_LOCAL_SERVER_ALIAS,
  LOCAL_ALIAS_KEY,
  validateServerAlias,
} from "./serverAliasModel";

export type StudioServer = {
  id: string;
  label: string;
  origin: string;
  credentialId?: string;
  workspaceId?: string;
  alias?: string;
};
export type RegistryStorage = Pick<Storage, "getItem">;
export const LOCAL_NAME_KEY = "studio-local-server-name-v1";
export const SERVER_REGISTRY_KEY = "studio-paired-servers-v1";
export const SERVER_REGISTRY_EVENT = "studio-server-registry";
export function serverResourceScope(server?: Pick<StudioServer, "id">) {
  return !server || server.id === "current" ? "local" : server.id;
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
export function parseServers(
  value: unknown,
  localAlias: string,
): StudioServer[] {
  if (!Array.isArray(value))
    throw new Error("The saved server list is invalid.");
  const ids = new Set<string>();
  const aliases = new Set<string>([localAlias]);
  for (const item of value) {
    if (item?.alias !== undefined) {
      validateServerAlias(item.alias, aliases);
      aliases.add(item.alias);
    }
  }
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
    const alias = item.alias || defaultServerAlias(item, aliases);
    aliases.add(alias);
    return {
      id: item.id,
      label: item.label,
      origin: serveOrigin(item.origin),
      credentialId: item.credentialId,
      alias,
      ...(typeof item.workspaceId === "string"
        ? { workspaceId: item.workspaceId }
        : {}),
    };
  });
}
export function readRegistryServers(storage: RegistryStorage) {
  return parseServers(
    JSON.parse(storage.getItem(SERVER_REGISTRY_KEY) || "[]"),
    storage.getItem(LOCAL_ALIAS_KEY) || DEFAULT_LOCAL_SERVER_ALIAS,
  );
}
export function normalizeServerAliases(
  servers: readonly StudioServer[],
  localAlias: string,
) {
  const aliases = new Set([localAlias]);
  for (const server of servers)
    if (server.alias) {
      validateServerAlias(server.alias, aliases);
      aliases.add(server.alias);
    }
  return servers.map((server) => {
    const alias = server.alias || defaultServerAlias(server, aliases);
    aliases.add(alias);
    return { ...server, alias };
  });
}
export function localServerMetadata(input: {
  label: string | null;
  alias: string | null;
  origin: string;
}): StudioServer {
  return {
    id: "local",
    label: input.label || "This computer",
    origin: input.origin,
    alias: input.alias || DEFAULT_LOCAL_SERVER_ALIAS,
  };
}
