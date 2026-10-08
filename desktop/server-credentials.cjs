const fs = require("node:fs/promises");
const path = require("node:path");
const crypto = require("node:crypto");
function serveOrigin(value) {
  const url = new URL(value);
  if (
    url.protocol !== "https:" ||
    !url.hostname.endsWith(".ts.net") ||
    url.port ||
    url.username ||
    url.password ||
    url.pathname !== "/" ||
    url.search ||
    url.hash
  )
    throw new Error("Use a Tailscale Serve HTTPS origin.");
  return url.origin;
}
function canonicalRequest(
  method,
  target,
  serverId,
  clientId,
  timestamp,
  nonce,
  requestId,
  bytes,
) {
  return [
    "studio-multi-server-v1",
    method.toUpperCase(),
    target,
    serverId,
    clientId,
    timestamp,
    nonce,
    requestId,
    crypto.createHash("sha256").update(bytes).digest("hex"),
  ].join("\n");
}
function createServerCredentials({
  profile,
  safeStorage,
  fetchRequest = fetch,
}) {
  const filename = path.join(profile, "paired-servers.json");
  let writing = Promise.resolve();
  function secure() {
    if (
      !safeStorage.isEncryptionAvailable() ||
      safeStorage.getSelectedStorageBackend?.() === "basic_text"
    )
      throw new Error(
        "Secure key storage is unavailable. Enable the system key store before pairing.",
      );
  }
  async function read() {
    secure();
    try {
      return JSON.parse(
        safeStorage.decryptString(
          Buffer.from(await fs.readFile(filename, "utf8"), "base64"),
        ),
      );
    } catch (error) {
      if (error.code === "ENOENT") return { drafts: {}, servers: {} };
      throw error;
    }
  }
  function change(update) {
    const work = writing.then(async () => {
      const state = await read();
      const result = await update(state);
      await fs.mkdir(profile, { recursive: true, mode: 0o700 });
      const temporary = filename + "." + crypto.randomUUID() + ".tmp";
      try {
        await fs.writeFile(
          temporary,
          safeStorage.encryptString(JSON.stringify(state)).toString("base64"),
          { mode: 0o600 },
        );
        await fs.rename(temporary, filename);
      } finally {
        await fs.rm(temporary, { force: true });
      }
      return result;
    });
    writing = work.catch(() => {});
    return work;
  }
  async function signed(credential, request, requestId) {
    const url = new URL(request.url);
    if (url.origin !== credential.origin || !url.pathname.startsWith("/api/"))
      throw new Error("The request does not belong to this server.");
    const bytes = Buffer.from(await request.clone().arrayBuffer());
    const timestamp = String(Math.floor(Date.now() / 1000));
    const nonce = crypto.randomBytes(24).toString("base64url");
    const headers = new Headers(request.headers);
    headers.delete("X-Canvas-Token");
    headers.delete("Cookie");
    headers.delete("Authorization");
    headers.set("X-Studio-Client", credential.clientId);
    headers.set("X-Studio-Server", credential.serverId);
    headers.set("X-Studio-Timestamp", timestamp);
    headers.set("X-Studio-Nonce", nonce);
    headers.set("X-Studio-Request-Id", requestId);
    headers.set(
      "X-Studio-Signature",
      crypto
        .sign(
          null,
          Buffer.from(
            canonicalRequest(
              request.method,
              url.pathname + url.search,
              credential.serverId,
              credential.clientId,
              timestamp,
              nonce,
              requestId,
              bytes,
            ),
          ),
          credential.privateKey,
        )
        .toString("base64"),
    );
    return fetchRequest(
      new Request(request, { headers, redirect: "error", credentials: "omit" }),
    );
  }
  return {
    async pair({ origin, invitation: input, requestId }) {
      origin = serveOrigin(origin);
      const invitation = input.invitation || input;
      if (
        invitation.protocol !== 1 ||
        !invitation.inviteId ||
        !invitation.token ||
        !invitation.serverId ||
        invitation.serverId === "local" ||
        !invitation.publicKey ||
        serveOrigin(invitation.origin) !== origin
      )
        throw new Error("The pairing invitation is invalid.");
      const draft = await change((state) => {
        if (state.drafts[requestId]) {
          const existing = state.drafts[requestId];
          if (
            existing.origin !== origin ||
            existing.serverId !== invitation.serverId ||
            existing.inviteId !== invitation.inviteId
          )
            throw new Error(
              "This request identity belongs to another pairing attempt.",
            );
          return existing;
        }
        const keys = crypto.generateKeyPairSync("ed25519");
        const clientId = crypto.randomUUID();
        const publicKey = keys.publicKey
          .export({ type: "spki", format: "pem" })
          .toString();
        const body = JSON.stringify({
          protocol: 1,
          inviteId: invitation.inviteId,
          token: invitation.token,
          clientId,
          label: "Studio UI",
          kind: "ui",
          publicKey,
          requestId,
        });
        const value = {
          origin,
          serverId: invitation.serverId,
          clientId,
          inviteId: invitation.inviteId,
          publicKey,
          privateKey: keys.privateKey
            .export({ type: "pkcs8", format: "pem" })
            .toString(),
          body,
        };
        state.drafts[requestId] = value;
        return value;
      });
      if (draft.receipt) return draft.receipt;
      const response = await signed(
        draft,
        new Request(origin + "/api/multi-server/v1/pair", {
          method: "POST",
          body: draft.body,
          headers: { "Content-Type": "application/json" },
          signal: AbortSignal.timeout(15000),
        }),
        requestId,
      );
      const value = await response.json();
      if (!response.ok)
        throw new Error(
          value.error ||
            `Pairing failed (${response.status}). Retry the same invitation.`,
        );
      if (
        value.protocol !== 1 ||
        value.paired !== true ||
        value.serverId !== invitation.serverId ||
        value.clientId !== draft.clientId ||
        serveOrigin(value.origin) !== origin ||
        value.publicKey !== invitation.publicKey ||
        value.tailscaleUser !== invitation.tailscaleUser
      )
        throw new Error(
          "The pairing response does not match the server invitation.",
        );
      const receipt = {
        id: value.serverId,
        origin,
        label: value.label || invitation.label || new URL(origin).hostname,
        credentialId: draft.clientId,
      };
      await change((state) => {
        state.servers[receipt.id] = {
          ...draft,
          body: undefined,
          targetPublicKey: value.publicKey,
          tailscaleUser: value.tailscaleUser,
        };
        state.drafts[requestId] = {
          ...draft,
          body: undefined,
          privateKey: undefined,
          receipt,
        };
      });
      return receipt;
    },
    async owns(serverId) {
      return !!(await read()).servers[serverId];
    },
    async request(
      {
        serverId,
        credentialId,
        url,
        method = "GET",
        headers = {},
        body,
        requestId,
      },
      signal,
    ) {
      const credential = (await read()).servers[serverId];
      if (!credential || credential.clientId !== credentialId)
        throw new Error("The server key is unavailable. Pair this UI again.");
      if (!["GET", "POST", "HEAD"].includes(method))
        throw new Error("Unsupported server method.");
      return signed(
        credential,
        new Request(url, {
          method,
          headers,
          ...(body?.byteLength ? { body } : {}),
          signal,
        }),
        requestId,
      );
    },
    async forget(serverId) {
      await change((state) => {
        delete state.servers[serverId];
        for (const [id, row] of Object.entries(state.drafts))
          if (row.serverId === serverId) delete state.drafts[id];
      });
    },
  };
}
module.exports = { createServerCredentials, canonicalRequest, serveOrigin };
