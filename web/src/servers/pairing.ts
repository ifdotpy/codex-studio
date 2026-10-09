import { serveOrigin, type StudioServer } from "./registry";
export class PairingNotAppliedError extends Error {
  constructor() {
    super(
      "The invitation is unavailable. Studio will request a new invitation.",
    );
  }
}
export type PairInvitation = {
  protocol: 1;
  inviteId: string;
  token: string;
  serverId: string;
  label: string;
  origin: string;
  publicKey: string;
  tailscaleUser: string;
  expires: number;
};
export type PairReceipt = {
  protocol: 1;
  serverId: string;
  clientId: string;
  label: string;
  origin: string;
  publicKey: string;
  tailscaleUser: string;
  paired: true;
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
    value.protocol !== 1 ||
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
export function pairedServer(
  receipt: PairReceipt,
  invitation: PairInvitation,
  clientId: string,
): StudioServer {
  if (
    receipt.protocol !== 1 ||
    receipt.paired !== true ||
    receipt.serverId !== invitation.serverId ||
    receipt.clientId !== clientId ||
    serveOrigin(receipt.origin) !== serveOrigin(invitation.origin) ||
    receipt.publicKey !== invitation.publicKey ||
    receipt.tailscaleUser !== invitation.tailscaleUser
  )
    throw new Error(
      "The pairing response does not match the server invitation.",
    );
  return {
    id: receipt.serverId,
    label:
      typeof receipt.label === "string" && receipt.label.trim()
        ? receipt.label.slice(0, 128)
        : invitation.label || new URL(receipt.origin).hostname,
    origin: serveOrigin(receipt.origin),
    credentialId: clientId,
  };
}
export function signatureInput(
  method: string,
  path: string,
  serverId: string,
  clientId: string,
  timestamp: string,
  nonce: string,
  requestId: string,
  hash: string,
) {
  return [
    "studio-multi-server-v1",
    method.toUpperCase(),
    path,
    serverId,
    clientId,
    timestamp,
    nonce,
    requestId,
    hash,
  ].join("\n");
}
export function publicKeyPem(bytes: ArrayBuffer) {
  const base64 = btoa(String.fromCharCode(...new Uint8Array(bytes)));
  return `-----BEGIN PUBLIC KEY-----\n${base64.match(/.{1,64}/g)!.join("\n")}\n-----END PUBLIC KEY-----\n`;
}
export async function signedRequest(
  request: Request,
  serverId: string,
  clientId: string,
  privateKey: CryptoKey,
  requestId: string,
) {
  const url = new URL(request.url);
  const body = new Uint8Array(await request.clone().arrayBuffer());
  const hash = Array.from(
    new Uint8Array(await crypto.subtle.digest("SHA-256", body)),
    (byte) => byte.toString(16).padStart(2, "0"),
  ).join("");
  const timestamp = String(Math.floor(Date.now() / 1000));
  const nonce = btoa(
    String.fromCharCode(...crypto.getRandomValues(new Uint8Array(24))),
  )
    .replace(/\+/g, "-")
    .replace(/\//g, "_")
    .replace(/=+$/, "");
  const signature = await crypto.subtle.sign(
    "Ed25519",
    privateKey,
    new TextEncoder().encode(
      signatureInput(
        request.method,
        url.pathname + url.search,
        serverId,
        clientId,
        timestamp,
        nonce,
        requestId,
        hash,
      ),
    ),
  );
  const headers = new Headers(request.headers);
  headers.delete("X-Canvas-Token");
  headers.delete("Cookie");
  headers.delete("Authorization");
  headers.set("X-Studio-Client", clientId);
  headers.set("X-Studio-Server", serverId);
  headers.set("X-Studio-Timestamp", timestamp);
  headers.set("X-Studio-Nonce", nonce);
  headers.set("X-Studio-Request-Id", requestId);
  headers.set(
    "X-Studio-Signature",
    btoa(String.fromCharCode(...new Uint8Array(signature))),
  );
  return new Request(request.clone(), {
    headers,
    credentials: "omit",
    redirect: "error",
  });
}
export async function requestIdentity(request: Request): Promise<string> {
  if (request.headers.get("X-Studio-Request-Id"))
    return request.headers.get("X-Studio-Request-Id")!;
  if (request.method === "GET" || request.method === "HEAD")
    return crypto.randomUUID();
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
      ].find((id) => typeof id === "string" && id) || crypto.randomUUID()
    );
  } catch {
    return crypto.randomUUID();
  }
}
