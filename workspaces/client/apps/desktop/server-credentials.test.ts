import assert from "node:assert/strict";
import { present } from "./test-support.ts";
import type { SafeStorage } from "electron";
import type { CredentialFetch } from "./server-credentials.ts";
import { expect, test } from "vitest";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import crypto from "node:crypto";
import {
  createServerCredentials,
  canonicalRequest,
  serveOrigin,
} from "./server-credentials.ts";
const safeStorage = {
  isEncryptionAvailable: () => true,
  encryptString: (text: string) => Buffer.from(text).reverse(),
  decryptString: (bytes: Buffer) => Buffer.from(bytes).reverse().toString(),
} satisfies Pick<
  SafeStorage,
  "isEncryptionAvailable" | "encryptString" | "decryptString"
>;
test("canonical request matches the frozen Rust signing contract", async () => {
  const vector = JSON.parse(
    await readFile(
      new URL(
        "../../../runtime/packages/runtime-services/tests/multi_server/signature.json",
        import.meta.url,
      ),
      "utf8",
    ),
  );
  const key = crypto.createPrivateKey({
    key: Buffer.from(
      "302e020100300506032b6570042204209d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60",
      "hex",
    ),
    format: "der",
    type: "pkcs8",
  });
  const signed = canonicalRequest(
    "POST",
    "/api/multi-server/v1/auto-pair",
    "peer-server",
    "rust-server",
    "1700000000",
    "abcdefghijklmnopqrstuvwxyz012345",
    "rust-request",
    Buffer.from('{"clientId":"rust-server","requestId":"rust-request"}'),
  );
  expect(crypto.sign(null, Buffer.from(signed), key).toString("base64")).toBe(
    vector.pythonSignature,
  );
});
test("pair retry keeps its key and body, signs exact bytes and refuses another server origin", async () => {
  const root = await mkdtemp(path.join(tmpdir(), "studio-server-keys-"));
  const invitation = {
    protocol: 1,
    serverId: "server-a",
    origin: "https://computer.tailnet.ts.net:8443",
    inviteId: "invite",
    token: "token-secret",
    publicKey: "server-public-key",
    tailscaleUser: "owner",
    label: "Computer",
  };
  const calls: { headers: Record<string, string>; text: string }[] = [];
  let failed = false;
  let ownKey = "";
  const fetchRequest: CredentialFetch = async (request) => {
    const text = await request.clone().text();
    calls.push({ headers: Object.fromEntries(request.headers), text });
    if (request.url.endsWith("/pair")) ownKey = JSON.parse(text).publicKey;
    const headers = request.headers;
    expect(headers.get("x-canvas-token")).toBeNull();
    expect(request.redirect).toBe("error");
    const signed = `studio-multi-server-v1
${request.method}
${new URL(request.url).pathname + new URL(request.url).search}
${headers.get("x-studio-server")}
${headers.get("x-studio-client")}
${headers.get("x-studio-timestamp")}
${headers.get("x-studio-nonce")}
${headers.get("x-studio-request-id")}
${crypto.createHash("sha256").update(text).digest("hex")}`;
    expect(
      crypto.verify(
        null,
        Buffer.from(signed),
        ownKey,
        Buffer.from(present(headers.get("x-studio-signature")), "base64"),
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
    const identity = {
      origin: invitation.origin,
      serverId: invitation.serverId,
      inviteId: invitation.inviteId,
      requestId: attempt.requestId,
    };
    expect(await adapter.hasPairAttempt(identity)).toBe(false);
    await expect(adapter.pair(attempt)).rejects.toThrow("response lost");
    expect(await adapter.hasPairAttempt(identity)).toBe(true);
    await expect(
      adapter.hasPairAttempt({ ...identity, serverId: "another-server" }),
    ).rejects.toThrow("another pairing attempt");
    adapter = createServerCredentials({
      profile: root,
      safeStorage,
      fetchRequest,
    });
    const server = await adapter.pair(attempt);
    expect(await adapter.hasPairAttempt(identity)).toBe(true);
    expect(present(calls[0]).text).toBe(present(calls[1]).text);
    expect(present(calls[0]).headers["x-studio-nonce"]).not.toBe(
      present(calls[1]).headers["x-studio-nonce"],
    );
    expect(present(calls[0]).headers["x-studio-request-id"]).toBe(
      present(calls[1]).headers["x-studio-request-id"],
    );
    await adapter.request({
      serverId: server.id,
      credentialId: server.credentialId,
      url: invitation.origin + "/api/search?q=a%20b",
      method: "GET",
      headers: [["X-Canvas-Token", "local-only"]],
      requestId: "read-once",
    });
    // The shell summary sends no request ID. The signed bytes and the header
    // must still carry the same one, or the server refuses the signature.
    await adapter.request({
      serverId: server.id,
      credentialId: server.credentialId,
      url: invitation.origin + "/api/ui-summary",
      method: "GET",
    });
    expect(present(calls.at(-1)).headers["x-studio-request-id"]).toMatch(
      /^[0-9a-f-]{36}$/,
    );
    await expect(
      adapter.request({
        serverId: server.id,
        credentialId: server.credentialId,
        url: invitation.origin + "/api/answer",
        method: "POST",
        body: new TextEncoder().encode("{}").buffer,
      }),
    ).rejects.toThrow("needs a request ID");
    await expect(
      adapter.request({
        serverId: server.id,
        credentialId: server.credentialId,
        url: "https://other.tailnet.ts.net/api/session",
        method: "GET",
        requestId: "read-other",
      }),
    ).rejects.toThrow("does not belong");
    await expect(
      adapter.request({
        serverId: server.id,
        credentialId: server.credentialId,
        url: "https://computer.tailnet.ts.net/api/session",
        method: "GET",
        requestId: "read-other-port",
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
test("Serve HTTPS ports retain the host, credential, and path restrictions", () => {
  expect(serveOrigin("https://computer.tailnet.ts.net:8443/")).toBe(
    "https://computer.tailnet.ts.net:8443",
  );
  expect(serveOrigin("https://computer.tailnet.ts.net:443/")).toBe(
    "https://computer.tailnet.ts.net",
  );
  for (const origin of [
    "http://computer.tailnet.ts.net:8443",
    "https://computer.tailnet.ts.net:0",
    "https://computer.tailnet.ts.net:65536",
    "https://computer.tailnet.ts.net.attacker.test:8443",
    "https://user:secret@computer.tailnet.ts.net:8443",
    "https://computer.tailnet.ts.net:8443/path",
    "https://computer.tailnet.ts.net:8443/?token=secret",
    "https://computer.tailnet.ts.net:8443/#fragment",
  ])
    expect(() => serveOrigin(origin)).toThrow();
});
test("a confirmed unavailable invitation releases only its rejected attempt and coalesces concurrent retries", async () => {
  const root = await mkdtemp(path.join(tmpdir(), "studio-rejected-invite-"));
  const invitation = {
    protocol: 1,
    serverId: "remote",
    origin: "https://remote.tailnet.ts.net",
    inviteId: "old",
    token: "secret",
    publicKey: "key",
    tailscaleUser: "owner",
  };
  let respond: (response: Response) => void = () => {};
  const pending = new Promise<Response>((resolve) => {
    respond = resolve;
  });
  const calls: Record<string, unknown>[] = [];
  let reject = true;
  const adapter = createServerCredentials({
    profile: root,
    safeStorage,
    fetchRequest: async (request) => {
      const body: unknown = await request.clone().json();
      assert(body && typeof body === "object");
      assert("inviteId" in body && "clientId" in body);
      calls.push({ ...body });
      if (reject) return pending;
      return Response.json({
        ...invitation,
        inviteId: body.inviteId,
        clientId: body.clientId,
        paired: true,
      });
    },
  });
  try {
    const input = {
      origin: invitation.origin,
      invitation,
      requestId: "old-request",
    };
    const first = adapter.pair(input);
    const duplicate = adapter.pair(input);
    expect(first).toBe(duplicate);
    await expect(
      adapter.pair({
        ...input,
        invitation: { ...invitation, inviteId: "other" },
      }),
    ).rejects.toThrow("another pairing attempt");
    respond(
      Response.json(
        {
          error: "The invitation is expired or was already used",
          code: "invite_unavailable",
        },
        { status: 403 },
      ),
    );
    await expect(first).rejects.toMatchObject({ code: "invite_unavailable" });
    expect(calls).toHaveLength(1);
    expect(
      await adapter.hasPairAttempt({
        origin: invitation.origin,
        serverId: "remote",
        requestId: "old-request",
        inviteId: "old",
      }),
    ).toBe(false);
    reject = false;
    await adapter.pair({
      ...input,
      invitation: { ...invitation, inviteId: "new" },
      requestId: "new-request",
    });
    expect(calls).toHaveLength(2);
    expect(present(calls[1]).clientId).not.toBe(present(calls[0]).clientId);
    expect(
      await adapter.hasPairAttempt({
        origin: invitation.origin,
        serverId: "remote",
        requestId: "new-request",
        inviteId: "new",
      }),
    ).toBe(true);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});
test("pairing fails when the OS key store is unavailable or uses plaintext", async () => {
  for (const store of [
    { ...safeStorage, isEncryptionAvailable: () => false },
    { ...safeStorage, getSelectedStorageBackend: () => "basic_text" as const },
  ]) {
    const adapter = createServerCredentials({
      profile: "/unused",
      safeStorage: store,
    });
    await expect(
      adapter.pair({
        origin: "https://computer.tailnet.ts.net",
        requestId: "pair",
        invitation: {
          protocol: 1,
          inviteId: "invite",
          token: "token",
          serverId: "server",
          publicKey: "public",
          origin: "https://computer.tailnet.ts.net",
        },
      }),
    ).rejects.toThrow("Secure key storage is unavailable");
  }
});

test("an unpaired profile never reads the system key store", async () => {
  const folder = await mkdtemp(path.join(tmpdir(), "studio-unpaired-"));
  let calls = 0;
  const adapter = createServerCredentials({
    profile: folder,
    safeStorage: {
      ...safeStorage,
      isEncryptionAvailable() {
        calls++;
        throw new Error("Unexpected key-store access");
      },
    },
  });
  try {
    expect(await adapter.owns("missing")).toBe(false);
    await expect(
      adapter.request({
        serverId: "missing",
        credentialId: "missing",
        url: "https://computer.tailnet.ts.net/api/session",
        method: "GET",
        requestId: "read",
      }),
    ).rejects.toThrow("The server key is unavailable");
    expect(calls).toBe(0);
  } finally {
    await rm(folder, { recursive: true, force: true });
  }
});
