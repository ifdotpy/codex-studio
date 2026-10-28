import { randomUUID } from "../uuid";
import { serveOrigin } from "./registry";
import { ACCESS_PROTOCOL_VERSION } from "../generated/multiServerProtocol";
export class PairingNotAppliedError extends Error {
  constructor() {
    super(
      "The invitation is unavailable. Studio will request a new invitation.",
    );
  }
}
export type PairInvitation = {
  protocol: typeof ACCESS_PROTOCOL_VERSION;
  inviteId: string;
  token: string;
  serverId: string;
  label: string;
  origin: string;
  publicKey: string;
  tailscaleUser: string;
  expires: number;
};
export function parseInvitation(code: string, origin: string): PairInvitation {
  const input: unknown = JSON.parse(code);
  const value = (
    input && typeof input === "object" && "invitation" in input
      ? input.invitation
      : input
  ) as PairInvitation;
  if (
    !value ||
    value.protocol !== ACCESS_PROTOCOL_VERSION ||
    typeof value.inviteId !== "string" ||
    !value.inviteId ||
    typeof value.token !== "string" ||
    !value.token ||
    typeof value.serverId !== "string" ||
    !/^[a-zA-Z0-9_-]{1,128}$/.test(value.serverId) ||
    value.serverId === "local" ||
    typeof value.publicKey !== "string" ||
    !value.publicKey.includes("BEGIN PUBLIC KEY") ||
    typeof value.tailscaleUser !== "string" ||
    !value.tailscaleUser ||
    serveOrigin(value.origin) !== serveOrigin(origin)
  )
    throw new Error(
      "The pairing invitation is invalid or belongs to another server.",
    );
  return value;
}
export async function requestIdentity(request: Request): Promise<string> {
  if (request.headers.get("X-Studio-Request-Id"))
    return request.headers.get("X-Studio-Request-Id")!;
  if (request.method === "GET" || request.method === "HEAD")
    return randomUUID();
  try {
    const value = await request.clone().json();
    return (
      [
        value.requestId,
        value.request_id,
        ...(["/api/messages", "/api/leads", "/api/assets"].includes(
          new URL(request.url).pathname,
        )
          ? [value.id]
          : []),
      ].find((id) => typeof id === "string" && id) || randomUUID()
    );
  } catch {
    return randomUUID();
  }
}
