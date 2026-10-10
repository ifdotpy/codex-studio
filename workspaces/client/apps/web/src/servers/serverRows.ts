import type { StudioServer } from "./registry";
import type { DiscoveredServer } from "./discoveryModel";
import type { ResourceConnectionState } from "../sync/resourceEvents";
export type ServerStatus =
  | "Active"
  | "Online"
  | "Paired"
  | "Discovered"
  | "Unreachable"
  | "Offline"
  | "Revoked"
  | "Update required";
export function serverRows(
  servers: StudioServer[],
  peers: DiscoveredServer[],
  statuses: Record<string, ResourceConnectionState>,
) {
  const rows = [
    ...servers,
    ...peers.filter((peer) => !servers.some((server) => server.id === peer.id)),
  ].map((server) => {
    const peer = peers.find((row) => row.id === server.id);
    const registered = servers.find((row) => row.id === server.id);
    const connection = statuses[server.id];
    const status: ServerStatus =
      peer?.status === "revoked"
        ? "Revoked"
        : connection === "live"
          ? "Active"
          : connection === "offline" || connection === "degraded"
            ? "Offline"
            : connection === "schema-mismatch"
              ? "Update required"
              : server.id === "local"
                ? "Online"
                : peer?.status === "discovered"
                  ? "Discovered"
                  : peer?.status === "unreachable"
                    ? "Unreachable"
                    : "Paired";
    return { server, peer, registered, status };
  });
  const rank: Record<ServerStatus, number> = {
    Active: 1,
    Online: 1,
    Paired: 1,
    Discovered: 2,
    Unreachable: 3,
    Offline: 3,
    Revoked: 4,
    "Update required": 3,
  };
  return rows.sort(
    (a, b) =>
      (a.server.id === "local" ? 0 : rank[a.status]) -
        (b.server.id === "local" ? 0 : rank[b.status]) ||
      a.server.label.localeCompare(b.server.label),
  );
}
export function relativeSeen(seconds: number, now = Date.now() / 1000) {
  const elapsed = Math.max(0, now - seconds);
  if (elapsed < 60) return "just now";
  if (elapsed < 3600) return `${Math.floor(elapsed / 60)} min ago`;
  if (elapsed < 86400) return `${Math.floor(elapsed / 3600)} h ago`;
  return `${Math.floor(elapsed / 86400)} d ago`;
}
