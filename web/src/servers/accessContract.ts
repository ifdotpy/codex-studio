import type { PairInvitation } from "./pairing";
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
};
export type ServerAccessState = {
  protocol: 1;
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
  settings?: { autoPair: boolean };
  servers: ServerAccessPeer[];
  invites: unknown[];
};
export type ServerAccessRequest =
  | { action: "create_invite"; requestId: string; label?: string }
  | { action: "revoke"; clientId: string; requestId: string }
  | { action: "unrevoke"; clientId: string; requestId: string }
  | { action: "discover"; requestId: string }
  | { action: "ui_invite"; serverId: string; requestId: string }
  | { action: "settings"; autoPair: boolean; requestId: string };
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
