import { expect, test } from "vitest";
import { createRequire } from "node:module";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import crypto from "node:crypto";
const require = createRequire(import.meta.url);
const {
  createServerCredentials,
  canonicalRequest,
} = require("./server-credentials.cjs");
const safeStorage = {
  isEncryptionAvailable: () => true,
  encryptString: (text) => Buffer.from(text).reverse(),
  decryptString: (bytes) => Buffer.from(bytes).reverse().toString(),
};
test("pair retry keeps its key and body, signs exact bytes and refuses another server origin", async () => {
  const root = await mkdtemp(path.join(tmpdir(), "studio-server-keys-"));
  const invitation = {
    protocol: 1,
    serverId: "server-a",
    origin: "https://computer.tailnet.ts.net",
    inviteId: "invite",
    token: "token-secret",
    publicKey: "server-public-key",
    tailscaleUser: "owner",
    label: "Computer",
  };
  const calls = [];
  let failed = false;
  let ownKey;
  const fetchRequest = async (request) => {
    const text = await request.clone().text();
    calls.push({ headers: Object.fromEntries(request.headers), text });
    if (request.url.endsWith("/pair")) ownKey = JSON.parse(text).publicKey;
    const headers = request.headers;
    expect(headers.get("x-canvas-token")).toBeNull();
    expect(request.redirect).toBe("error");
    const signed = canonicalRequest(
      request.method,
      new URL(request.url).pathname + new URL(request.url).search,
      headers.get("x-studio-server"),
      headers.get("x-studio-client"),
      headers.get("x-studio-timestamp"),
      headers.get("x-studio-nonce"),
      headers.get("x-studio-request-id"),
      Buffer.from(text),
    );
    expect(
      crypto.verify(
        null,
        Buffer.from(signed),
        ownKey,
        Buffer.from(headers.get("x-studio-signature"), "base64"),
      ),
    ).toBe(true);
    if (!failed) {
      failed = true;
      throw new TypeError("response lost");
    }
    if (request.url.endsWith("/pair"))
      return new Response(
        JSON.stringify({
          ...invitation,
          clientId: JSON.parse(text).clientId,
          paired: true,
        }),
      );
    return new Response("{}");
  };
  try {
    let adapter = createServerCredentials({
      profile: root,
      safeStorage,
      fetchRequest,
    });
    const attempt = {
      origin: invitation.origin,
      invitation,
      requestId: "pair-once",
    };
    await expect(adapter.pair(attempt)).rejects.toThrow("response lost");
    adapter = createServerCredentials({
      profile: root,
      safeStorage,
      fetchRequest,
    });
    const server = await adapter.pair(attempt);
    expect(calls[0].text).toBe(calls[1].text);
    expect(calls[0].headers["x-studio-nonce"]).not.toBe(
      calls[1].headers["x-studio-nonce"],
    );
    expect(calls[0].headers["x-studio-request-id"]).toBe(
      calls[1].headers["x-studio-request-id"],
    );
    await adapter.request({
      serverId: server.id,
      credentialId: server.credentialId,
      url: invitation.origin + "/api/search?q=a%20b",
      headers: { "X-Canvas-Token": "local-only" },
      requestId: "read-once",
    });
    await expect(
      adapter.request({
        serverId: server.id,
        credentialId: server.credentialId,
        url: "https://other.tailnet.ts.net/api/session",
        requestId: "read-other",
      }),
    ).rejects.toThrow("does not belong");
    expect(await adapter.owns(server.id)).toBe(true);
    const bytes = await readFile(
      path.join(root, "paired-servers.json"),
      "utf8",
    );
    expect(bytes).not.toContain("BEGIN PRIVATE KEY");
    expect(bytes).not.toContain("token-secret");
    await adapter.forget(server.id);
    expect(await adapter.owns(server.id)).toBe(false);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});
test("pairing fails when the OS key store is unavailable or uses plaintext", async () => {
  for (const store of [
    { isEncryptionAvailable: () => false },
    { ...safeStorage, getSelectedStorageBackend: () => "basic_text" },
  ]) {
    const adapter = createServerCredentials({
      profile: "/unused",
      safeStorage: store,
    });
    await expect(adapter.owns("any")).rejects.toThrow(
      "Secure key storage is unavailable",
    );
  }
});
