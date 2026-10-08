import { expect, it } from "vitest";
import { createPublicKey, verify } from "node:crypto";
import {
  parseInvitation,
  pairedServer,
  signedRequest,
  publicKeyPem,
  signatureInput,
} from "./pairing";
it("signs the exact encoded path, body and request identity without the local token", async () => {
  const keys = await crypto.subtle.generateKey({ name: "Ed25519" }, false, [
    "sign",
    "verify",
  ]);
  expect(keys.privateKey.extractable).toBe(false);
  const request = new Request(
    "https://computer.tailnet.ts.net/api/messages?q=a%20b",
    {
      method: "POST",
      body: '{"requestId":"once","text":"hello"}',
      headers: {
        "X-Canvas-Token": "local-secret",
        "X-Canvas-Workspace": "workspace",
      },
    },
  );
  const signed = await signedRequest(
    request,
    "server",
    "client",
    keys.privateKey,
    "once",
  );
  expect(signed.headers.get("X-Canvas-Token")).toBeNull();
  expect(signed.headers.get("X-Canvas-Workspace")).toBe("workspace");
  expect(signed.credentials).toBe("omit");
  expect(signed.redirect).toBe("error");
  const bytes = await request.clone().arrayBuffer();
  const hash = Buffer.from(
    await crypto.subtle.digest("SHA-256", bytes),
  ).toString("hex");
  const headers = signed.headers;
  const content = signatureInput(
    request.method,
    "/api/messages?q=a%20b",
    "server",
    "client",
    headers.get("X-Studio-Timestamp")!,
    headers.get("X-Studio-Nonce")!,
    "once",
    hash,
  );
  const key = createPublicKey(
    publicKeyPem(await crypto.subtle.exportKey("spki", keys.publicKey)),
  );
  expect(
    verify(
      null,
      Buffer.from(content),
      key,
      Buffer.from(headers.get("X-Studio-Signature")!, "base64"),
    ),
  ).toBe(true);
  const retry = await signedRequest(
    request,
    "server",
    "client",
    keys.privateKey,
    "once",
  );
  expect(retry.headers.get("X-Studio-Nonce")).not.toBe(
    headers.get("X-Studio-Nonce"),
  );
  expect(await retry.text()).toBe(await signed.text());
});
it("pins pairing to the invitation's server identity, owner and public key", () => {
  const invitation = {
    protocol: 1 as const,
    inviteId: "invite",
    token: "code",
    serverId: "server-a",
    origin: "https://computer.tailnet.ts.net",
    label: "Computer",
    publicKey: "-----BEGIN PUBLIC KEY-----\nabc\n-----END PUBLIC KEY-----\n",
    tailscaleUser: "owner",
    expires: 1,
  };
  expect(
    parseInvitation(JSON.stringify({ invitation }), invitation.origin),
  ).toEqual(invitation);
  const receipt = { ...invitation, clientId: "client", paired: true as const };
  expect(pairedServer(receipt, invitation, "client").id).toBe("server-a");
  expect(() =>
    pairedServer({ ...receipt, publicKey: "other" }, invitation, "client"),
  ).toThrow();
  expect(() =>
    pairedServer({ ...receipt, tailscaleUser: "other" }, invitation, "client"),
  ).toThrow();
});
