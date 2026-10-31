import type { DiscoveredServer } from "./discoveryModel";
import type { StudioServer } from "./registry";

export function accountServers(
  registered: StudioServer[],
  peers: DiscoveredServer[],
): StudioServer[] {
  return registered.filter(
    (server) =>
      !peers.some((peer) => peer.id === server.id && peer.status === "revoked"),
  );
}
