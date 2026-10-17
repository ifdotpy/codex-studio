import {
  readServers,
  writeServers,
  SERVER_REGISTRY_EVENT,
  SERVER_REGISTRY_KEY,
  type StudioServer,
} from "./registry";
import { serverCredentialAdapter } from "./transport";

const SERVER_STATUS_REFRESH_INTERVAL_MS = 30_000;

export function validateServerName(label: string) {
  if (
    Array.from(label).length < 1 ||
    Array.from(label).length > 80 ||
    label !== label.trim() ||
    /[\p{C}\p{Zl}\p{Zp}\t\n\r\v\f]/u.test(label) ||
    /[^ ]/u.test(label.replace(/[^\s]/gu, ""))
  )
    throw new Error("Use 1 to 80 visible characters without outer spaces.");
}

export async function refreshServerName(server: StudioServer) {
  const response = await serverCredentialAdapter().fetch(
    server,
    new Request(server.origin + "/api/multi-server/v1/status", {
      signal: AbortSignal.timeout(15000),
    }),
  );
  if (!response.ok) {
    await response.body?.cancel().catch(() => {});
    throw new Error("The server name is unavailable.");
  }
  const identity = await response.json();
  if (identity.serverId !== server.id)
    throw new Error("The server response has another identity.");
  validateServerName(identity.label);
  const rows = readServers();
  if (
    rows.some(
      (row) =>
        row.id === server.id &&
        row.origin === server.origin &&
        row.credentialId === server.credentialId &&
        row.label !== identity.label,
    )
  )
    writeServers(
      rows.map((row) =>
        row.id === server.id &&
        row.origin === server.origin &&
        row.credentialId === server.credentialId
          ? { ...row, label: identity.label }
          : row,
      ),
    );
}

/** Refresh registered peer names on change and periodically while peers exist. */
export function watchRegisteredServerNames(
  refresh: (server: StudioServer) => Promise<void> = refreshServerName,
) {
  let stopped = false;
  let refreshing = false;
  let refreshQueued = false;
  let timer: ReturnType<typeof setTimeout> | undefined;
  let servers = readRoster();

  const clearTimer = () => {
    if (timer !== undefined) clearTimeout(timer);
    timer = undefined;
  };
  const schedule = () => {
    clearTimer();
    if (stopped || servers.length === 0) return;
    timer = setTimeout(
      () => void refreshKnownServers(),
      SERVER_STATUS_REFRESH_INTERVAL_MS,
    );
  };
  const refreshKnownServers = async () => {
    clearTimer();
    if (stopped || servers.length === 0) return;
    if (refreshing) {
      refreshQueued = true;
      return;
    }
    refreshing = true;
    try {
      for (const server of servers) {
        if (stopped) break;
        if (
          !servers.some(
            (current) =>
              current.id === server.id &&
              current.origin === server.origin &&
              current.credentialId === server.credentialId,
          )
        )
          continue;
        try {
          await refresh(server);
        } catch {
          // Offline peers are retried on the next registry event or interval.
        }
      }
    } finally {
      refreshing = false;
      schedule();
      if (refreshQueued && !stopped) {
        refreshQueued = false;
        void refreshKnownServers();
      }
    }
  };
  const onRegistryChange = () => {
    const next = readRoster();
    const changed = !sameRefreshRoster(servers, next);
    servers = next;
    if (servers.length === 0) clearTimer();
    else if (changed) void refreshKnownServers();
  };
  const onStorage = (event: StorageEvent) => {
    if (event.key === SERVER_REGISTRY_KEY || event.key === null)
      onRegistryChange();
  };

  window.addEventListener(SERVER_REGISTRY_EVENT, onRegistryChange);
  window.addEventListener("storage", onStorage);
  if (servers.length > 0) void refreshKnownServers();

  return () => {
    stopped = true;
    refreshQueued = false;
    clearTimer();
    window.removeEventListener(SERVER_REGISTRY_EVENT, onRegistryChange);
    window.removeEventListener("storage", onStorage);
  };
}

function readRoster() {
  try {
    return readServers();
  } catch {
    return [];
  }
}

function sameRefreshRoster(
  left: readonly StudioServer[],
  right: readonly StudioServer[],
) {
  return (
    left.length === right.length &&
    left.every((server) =>
      right.some(
        (other) =>
          other.id === server.id &&
          other.origin === server.origin &&
          other.credentialId === server.credentialId,
      ),
    )
  );
}
