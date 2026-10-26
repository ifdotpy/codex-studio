import type { NativeServerContext } from "./client";
import type { NativeServiceReady } from "./services/types";
import {
  compareNativeRevisions,
  readNativeRevision,
} from "./services/revision";
import {
  LOCAL_NAME_KEY,
  SERVER_REGISTRY_KEY,
  parseServers,
  type StudioServer,
} from "../servers/registryModel";
import {
  DEFAULT_LOCAL_SERVER_ALIAS,
  LOCAL_ALIAS_KEY,
  validateServerAlias,
} from "../servers/serverAliasModel";
export * from "../servers/registryModel";
export { shareServers } from "../servers/registrySharing";

/** Required host vault metadata. Preferences never authorize a pairing or transport. */
export type NativeRegistrySnapshot = Readonly<{
  serverId: string;
  workspaceId: string;
  revision: string;
  local: StudioServer;
  servers: readonly StudioServer[];
}>;
export type NativeRegistryMetadataPort = Readonly<{
  read(context: NativeServerContext): Promise<unknown>;
  subscribe(
    context: NativeServerContext,
    receive: (snapshot: unknown) => void,
    failed: (error: unknown) => void,
  ): Promise<() => Promise<void>>;
}>;

function object(value: unknown): value is Record<string, unknown> {
  return !!value && typeof value === "object" && !Array.isArray(value);
}
function sameTransport(a: StudioServer, b: StudioServer) {
  return (
    a.id === b.id &&
    a.origin === b.origin &&
    a.credentialId === b.credentialId &&
    a.workspaceId === b.workspaceId
  );
}
const instances = new WeakSet<object>();

/** Capture one ready native owner before the screen reads any registry metadata. */
export async function createNativeRegistry(
  ready: NativeServiceReady,
  options: {
    metadata: NativeRegistryMetadataPort;
    report(error: unknown): void;
  },
) {
  const { context, storage } = ready;
  storage.assertOwner(context);
  const metadata = options.metadata;
  if (
    !metadata ||
    typeof metadata.read !== "function" ||
    typeof metadata.subscribe !== "function"
  )
    throw new Error("The native host registry metadata port is unavailable.");
  const scope = Object.freeze({
    handle: context.handle,
    serverId: context.serverId,
    workspaceId: context.workspaceId,
    origin: context.origin,
    credentialId: context.credentialId,
  });
  let closed = false;
  let failure: unknown;
  const listeners = new Set<() => void>();
  function verify() {
    storage.assertOwner(context);
    if (
      context.handle !== scope.handle ||
      context.serverId !== scope.serverId ||
      context.workspaceId !== scope.workspaceId ||
      context.origin !== scope.origin ||
      context.credentialId !== scope.credentialId
    )
      throw new Error("The native registry context scope changed.");
  }
  function checked(value: unknown): NativeRegistrySnapshot {
    verify();
    if (
      !object(value) ||
      value.serverId !== scope.serverId ||
      value.workspaceId !== scope.workspaceId ||
      !object(value.local) ||
      value.local.id !== "local" ||
      typeof value.local.label !== "string" ||
      !value.local.label.trim() ||
      typeof value.local.origin !== "string" ||
      !value.local.origin ||
      typeof value.local.workspaceId !== "string" ||
      !/^[a-f0-9]{32}$/.test(value.local.workspaceId) ||
      (value.local.credentialId !== undefined &&
        value.local.credentialId !== null)
    )
      throw new Error(
        "The native registry snapshot has another owner or identity.",
      );
    const local: StudioServer = {
      id: "local",
      label: value.local.label,
      origin: value.local.origin,
      workspaceId: value.local.workspaceId,
    };
    if (value.local.alias !== undefined) {
      if (typeof value.local.alias !== "string")
        throw new Error("The native registry alias is invalid.");
      local.alias = validateServerAlias(value.local.alias);
    }
    const servers = parseServers(
      value.servers,
      local.alias || DEFAULT_LOCAL_SERVER_ALIAS,
    );
    const selected =
      scope.serverId === "local"
        ? local
        : servers.find((row) => row.id === scope.serverId);
    if (
      !selected ||
      selected.origin !== scope.origin ||
      selected.workspaceId !== scope.workspaceId ||
      (selected.credentialId ?? null) !== scope.credentialId
    )
      throw new Error(
        "The native registry transport identity does not match its context.",
      );
    return Object.freeze({
      serverId: scope.serverId,
      workspaceId: scope.workspaceId,
      revision: readNativeRevision(value.revision),
      local,
      servers,
    });
  }
  let current = checked(await metadata.read(context));
  const publish = () => {
    for (const listener of listeners) listener();
  };
  function failed(error: unknown) {
    if (closed) return;
    failure = error;
    options.report(error);
    publish();
  }
  function accept(value: unknown) {
    if (closed) return;
    const next = checked(value);
    const order = compareNativeRevisions(next.revision, current.revision);
    if (order < 0) return;
    if (order === 0 && JSON.stringify(next) !== JSON.stringify(current))
      throw new Error("The native registry revision has conflicting metadata.");
    current = next;
    failure = undefined;
    publish();
  }
  function projected() {
    verify();
    if (closed) throw new Error("The native registry is closed.");
    if (failure) throw failure;
    const local = {
      ...current.local,
      label: storage.getItem(LOCAL_NAME_KEY) || current.local.label,
      alias:
        storage.getItem(LOCAL_ALIAS_KEY) ||
        current.local.alias ||
        DEFAULT_LOCAL_SERVER_ALIAS,
    };
    validateServerAlias(local.alias);
    const host = new Map(current.servers.map((row) => [row.id, row]));
    const preferences = parseServers(
      JSON.parse(storage.getItem(SERVER_REGISTRY_KEY) || "[]"),
      local.alias,
    );
    const rows: StudioServer[] = [];
    for (const preference of preferences) {
      const owner = host.get(preference.id);
      if (!owner) continue;
      if (
        preference.origin !== owner.origin ||
        preference.credentialId !== owner.credentialId ||
        (preference.workspaceId !== undefined &&
          preference.workspaceId !== owner.workspaceId)
      )
        throw new Error(
          "The saved server display metadata has another host identity.",
        );
      rows.push({ ...owner, label: preference.label, alias: preference.alias });
      host.delete(owner.id);
    }
    rows.push(...host.values());
    return { local, servers: parseServers(rows, local.alias) };
  }
  const stopPreferences = storage.subscribe((key) => {
    if (
      closed ||
      (key !== null &&
        ![LOCAL_NAME_KEY, LOCAL_ALIAS_KEY, SERVER_REGISTRY_KEY].includes(key))
    )
      return;
    try {
      projected();
      publish();
    } catch (error) {
      failed(error);
    }
  });
  let stopHost: (() => Promise<void>) | undefined;
  try {
    stopHost = await metadata.subscribe(
      context,
      (value) => {
        try {
          accept(value);
        } catch (error) {
          failed(error);
        }
      },
      failed,
    );
    // Close the read/subscribe race; the host revision rejects older snapshots.
    accept(await metadata.read(context));
    projected();
  } catch (error) {
    closed = true;
    stopPreferences();
    await stopHost?.();
    throw error;
  }
  const registry = Object.freeze({
    read: () => projected().servers,
    local: () => projected().local,
    view(id: string) {
      const value = projected();
      const row =
        id === "local"
          ? value.local
          : value.servers.find((row) => row.id === id);
      if (!row)
        throw new Error("This server was removed. Select a paired server.");
      return row;
    },
    aliases() {
      const value = projected();
      return Object.fromEntries(
        [value.local, ...value.servers].map((row) => [
          row.id,
          row.alias || DEFAULT_LOCAL_SERVER_ALIAS,
        ]),
      );
    },
    async write(servers: StudioServer[]) {
      verify();
      accept(await metadata.read(context));
      projected();
      const localAlias =
        storage.getItem(LOCAL_ALIAS_KEY) ||
        current.local.alias ||
        DEFAULT_LOCAL_SERVER_ALIAS;
      const next = parseServers(servers, localAlias);
      if (
        next.length !== current.servers.length ||
        next.some((row) => {
          const owner = current.servers.find((host) => host.id === row.id);
          return !owner || !sameTransport(row, owner);
        })
      )
        throw new Error(
          "Server membership and transport identity require the native host registry.",
        );
      storage.setItem(SERVER_REGISTRY_KEY, JSON.stringify(next));
      await storage.flush();
      verify();
    },
    subscribe(changed: () => void) {
      projected();
      listeners.add(changed);
      return () => {
        listeners.delete(changed);
      };
    },
    assertStorage(candidate?: Storage) {
      verify();
      if (candidate && candidate !== storage)
        throw new Error("The native registry storage has another owner.");
    },
    async close() {
      if (closed) return;
      closed = true;
      stopPreferences();
      listeners.clear();
      await stopHost?.();
    },
  });
  instances.add(registry);
  return registry;
}
export type NativeRegistry = Awaited<ReturnType<typeof createNativeRegistry>>;
let installed: NativeRegistry | undefined;
export function installNativeRegistry(registry: NativeRegistry) {
  if (!instances.has(registry) || (installed && installed !== registry))
    throw new Error("The native registry has another owner.");
  registry.read();
  installed = registry;
  return () => {
    if (installed === registry) installed = undefined;
  };
}
export function nativeRegistry() {
  if (!installed) throw new Error("The native server registry is not ready.");
  return installed;
}
export function readServers(storage?: Storage) {
  const registry = nativeRegistry();
  registry.assertStorage(storage);
  return registry.read();
}
export function writeServers(servers: StudioServer[], storage?: Storage) {
  const registry = nativeRegistry();
  registry.assertStorage(storage);
  return registry.write(servers);
}
export const localServer = () => nativeRegistry().local();
export const viewServer = (id: string) => nativeRegistry().view(id);
export const readServerAliases = () => nativeRegistry().aliases();
