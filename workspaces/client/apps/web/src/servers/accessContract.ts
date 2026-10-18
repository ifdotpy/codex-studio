import type { PairInvitation } from "./pairing";
import { ACCESS_PROTOCOL_VERSION } from "../generated/multiServerProtocol";
export type ServerAccessPeer = {
  id: string;
  clientId: string;
  serverId: string | null;
  label: string;
  origin: string | null;
  publicKey: string;
  tailscaleUser: string;
  status: "discovered" | "paired" | "revoked" | "unreachable";
  reachability?: "reachable" | "unreachable" | "unknown" | null;
  created: number;
  lastSeen: number | null;
  autoPair: boolean | null;
  alias?: string | null;
  replacedBy?: string | null;
};
/** A server identity change that leaves live references to the lost identity. */
export type ServerIdentityAlert = {
  code: "peer_identity_changed" | "local_identity_changed";
  oldServerId: string;
  newServerId: string;
  origin: string | null;
  detail: string;
};
export type ServerAccessState = {
  protocol: typeof ACCESS_PROTOCOL_VERSION;
  identity: {
    serverId: string;
    label: string;
    origin: string | null;
    publicKey: string;
    tailscaleUser: string | null;
  };
  clients: {
    clientId: string;
    label: string;
    revoked?: number | null;
    status: "paired" | "revoked";
    created?: number;
  }[];
  settings?: {
    autoPair: boolean;
    autoUpdate?: boolean;
    aliases?: Record<string, string>;
  };
  servers: ServerAccessPeer[];
  invites: unknown[];
  identityAlerts?: ServerIdentityAlert[];
};
export type ServerAccessRequest =
  | { action: "create_invite"; requestId: string; label?: string }
  | { action: "revoke"; clientId: string; requestId: string }
  | { action: "unrevoke"; clientId: string; requestId: string }
  | { action: "discover"; requestId: string }
  | { action: "ui_invite"; serverId: string; requestId: string }
  | { action: "settings"; autoPair: boolean; requestId: string }
  | { action: "settings"; autoUpdate: boolean; requestId: string }
  | { action: "name"; serverId: string; label: string; requestId: string }
  | { action: "alias"; serverId: string; alias: string; requestId: string }
  | {
      action: "replace_server";
      oldServerId: string;
      newServerId: string;
      requestId: string;
    };
export type ServerAccessResponse =
  | ServerAccessState
  | { invitation: PairInvitation; expires: number }
  | { revoked: boolean };
export type ServerAccessPaths = {
  "/api/multi-server": {
    get: {
      responses: {
        200: { content: { "application/json": ServerAccessState } };
      };
    };
    post: {
      requestBody: { content: { "application/json": ServerAccessRequest } };
      responses: {
        200: { content: { "application/json": ServerAccessResponse } };
      };
    };
  };
};
