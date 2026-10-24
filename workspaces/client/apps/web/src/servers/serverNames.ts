import { readServers, writeServers, type StudioServer } from "./registry";
import { serverCredentialAdapter } from "./transport";

import type { ResourceRef, ResourceVersion } from "../sync/resourceEvents";

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
  applyServerName(server, identity.label);
}

function applyServerName(server: StudioServer, label: string) {
  validateServerName(label);
  const rows = readServers();
  if (
    rows.some(
      (row) =>
        row.id === server.id &&
        row.origin === server.origin &&
        row.credentialId === server.credentialId &&
        row.label !== label,
    )
  )
    writeServers(
      rows.map((row) =>
        row.id === server.id &&
        row.origin === server.origin &&
        row.credentialId === server.credentialId
          ? { ...row, label }
          : row,
      ),
    );
}

/** Each saved server owns its name through its existing discovery stream. */
export function watchServerName(
  server: StudioServer,
  events: {
    watchResourceChanges: (
      resource: ResourceRef,
      accept: (version?: ResourceVersion) => void,
    ) => () => void;
  },
) {
  let active = true;
  const stop = events.watchResourceChanges(
    { kind: "multi-server" },
    (version) => {
      const identity = version?.discoverySnapshot?.identity;
      if (!active || !identity || identity.serverId !== server.id) return;
      applyServerName(server, identity.label);
    },
  );
  return () => {
    active = false;
    stop();
  };
}
