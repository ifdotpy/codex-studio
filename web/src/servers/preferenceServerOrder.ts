import { readPreferenceFields } from "../sync/uiPreferenceStore";
import type { StudioServer } from "./registry";

export function preferenceServerIdentity(server: StudioServer): string {
  return (
    server.workspaceId ||
    localStorage.getItem(`studio-server-workspace-id:${server.id}`) ||
    server.id
  );
}

export function orderPreferenceServers(
  servers: StudioServer[],
): StudioServer[] {
  const order =
    readPreferenceFields("user")[JSON.stringify(["server-order"])]?.value;
  if (!Array.isArray(order)) return servers;
  const rank = (server: StudioServer) => {
    const index = order.indexOf(preferenceServerIdentity(server));
    return index < 0 ? Number.MAX_SAFE_INTEGER : index;
  };
  return [...servers].sort((a, b) => rank(a) - rank(b));
}
