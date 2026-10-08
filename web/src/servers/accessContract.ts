import type { PairInvitation } from "./pairing";
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
  servers: unknown[];
  invites: unknown[];
};
export type ServerAccessRequest =
  | { action: "create_invite"; requestId: string; label?: string }
  | { action: "revoke"; clientId: string; requestId: string };
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
