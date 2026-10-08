// The wire adapter maps the local server's discovery snapshot to this view.
export type DiscoveredServer = {
  id: string;
  label: string;
  origin: string;
  status: "discovered" | "paired" | "revoked" | "unreachable";
  lastSeen: number | null;
  paired: boolean;
  publicKey: string;
  generation: string;
};
export type DiscoverySnapshot = {
  localServerId: string;
  localLabel?: string;
  localOrigin?: string | null;
  autoPair: boolean;
  servers: DiscoveredServer[];
};
export type DiscoveryActions = {
  find: () => void;
  setAutoPair: (enabled: boolean) => void;
};

import type { ServerAccessState } from "./accessContract";
import { serveOrigin } from "./registry";
export function discoverySnapshot(
  state: ServerAccessState,
): DiscoverySnapshot | null {
  if (!state.settings) return null;
  return {
    localServerId: state.identity.serverId,
    localLabel: state.identity.label,
    localOrigin: state.identity.origin,
    autoPair: state.settings.autoPair,
    servers: state.servers
      .filter(
        (peer) =>
          peer.origin &&
          peer.serverId &&
          peer.serverId !== state.identity.serverId,
      )
      .map((peer) => ({
        id: peer.serverId!,
        label: peer.label,
        origin: serveOrigin(peer.origin!),
        status: peer.status,
        lastSeen: peer.lastSeen ?? null,
        paired: peer.status === "paired",
        publicKey: peer.publicKey,
        generation: JSON.stringify([peer.created, peer.publicKey]),
      })),
  };
}
