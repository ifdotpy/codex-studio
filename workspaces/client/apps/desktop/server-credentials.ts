import type { StudioServerIdentity } from "../web/src/servers/serverIdentity.ts";
import type { components } from "../web/src/generated/api.ts";
import type { SafeStorage } from "electron";
import type { ServerRequest, PairRequest } from "./api.ts";
export type Invitation = components["schemas"]["AccessInvitation"];
export type PairReceipt = Required<
  Pick<StudioServerIdentity, "id" | "origin" | "label" | "credentialId">
>;
export type PairInput = PairRequest;
type Draft = {
  origin: string;
  serverId: string;
  clientId: string;
  inviteId: string;
  publicKey: string;
  privateKey?: string | undefined;
  body?: string | undefined;
  receipt?: PairReceipt;
  rejected?: boolean;
  targetPublicKey?: string;
  tailscaleUser?: string;
};
type CredentialState = {
  drafts: Record<string, Draft>;
  servers: Record<string, Draft>;
};
export type CredentialFetch = (request: Request) => Promise<Response>;
import * as fs from "node:fs/promises";
import * as path from "node:path";
import * as crypto from "node:crypto";
function serveOrigin(value: string) {
  const url = new URL(value);
  if (
    url.protocol !== "https:" ||
    !url.hostname.endsWith(".ts.net") ||
    url.port === "0" ||
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
  method: string,
  target: string,
  serverId: string,
  clientId: string,
  timestamp: string,
  nonce: string,
  requestId: string,
  bytes: Parameters<import("node:crypto").Hash["update"]>[0],
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
}: {
  profile: string;
  safeStorage: Pick<
    SafeStorage,
    "isEncryptionAvailable" | "encryptString" | "decryptString"
  > &
    Partial<Pick<SafeStorage, "getSelectedStorageBackend">>;
  fetchRequest?: CredentialFetch;
}) {
  const filename = path.join(profile, "paired-servers.json");
  let writing: Promise<unknown> = Promise.resolve();
  const pairing = new Map<
    string,
    { identity: string; promise: Promise<PairReceipt> }
  >();
  function secure() {
    if (
      !safeStorage.isEncryptionAvailable() ||
      safeStorage.getSelectedStorageBackend?.() === "basic_text"
    )
      throw new Error(
        "Secure key storage is unavailable. Enable the system key store before pairing.",
      );
  }
  async function read(): Promise<CredentialState> {
    let encrypted;
    try {
      encrypted = await fs.readFile(filename, "utf8");
    } catch (caughterror) {
      if (!(caughterror instanceof Error)) throw caughterror;
      const error: NodeJS.ErrnoException = caughterror;
      if (error.code === "ENOENT") return { drafts: {}, servers: {} };
      throw error;
    }
    secure();
    // The private paired-server document is written only by change() and encrypted by secure storage.
    return JSON.parse(
      safeStorage.decryptString(Buffer.from(encrypted, "base64")),
    );
  }
  function change<T>(update: (state: CredentialState) => T | Promise<T>) {
    const work = writing.then(async () => {
      const state = await read();
      secure();
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
  async function signed(
    credential: Draft,
    request: Request,
    requestId: string,
  ) {
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
          credential.privateKey ?? "",
        )
        .toString("base64"),
    );
    return fetchRequest(
      new Request(request, { headers, redirect: "error", credentials: "omit" }),
    );
  }
  return {
    async hasPairAttempt({
      origin,
      serverId,
      requestId,
      inviteId,
    }: {
      origin: string;
      serverId: string;
      requestId: string;
      inviteId?: string | undefined;
    }) {
      origin = serveOrigin(origin);
      const state = await read();
      const draft = state.drafts[requestId];
      if (!draft) return false;
      if (
        draft.origin !== origin ||
        draft.serverId !== serverId ||
        (inviteId && draft.inviteId !== inviteId)
      )
        throw new Error(
          "The saved credential belongs to another pairing attempt.",
        );
      return !!draft.receipt || (!draft.rejected && !!draft.body);
    },
    pair(inputValue: PairInput) {
      const identity = JSON.stringify(inputValue);
      const existing = pairing.get(inputValue.requestId);
      if (existing) {
        if (existing.identity !== identity)
          return Promise.reject(
            new Error(
              "This request identity belongs to another pairing attempt.",
            ),
          );
        return existing.promise;
      }
      const promise = (async () => {
        let { origin } = inputValue;
        const { invitation: input, requestId } = inputValue;
        origin = serveOrigin(origin);
        const raw =
          input && typeof input === "object" && "invitation" in input
            ? input.invitation
            : input;
        if (!raw || typeof raw !== "object")
          throw new Error("The pairing invitation is invalid.");
        const invitation = raw as Record<string, unknown>;
        if (
          invitation.protocol !== 1 ||
          typeof invitation.inviteId !== "string" ||
          !invitation.inviteId ||
          typeof invitation.token !== "string" ||
          !invitation.token ||
          typeof invitation.serverId !== "string" ||
          !invitation.serverId ||
          invitation.serverId === "local" ||
          typeof invitation.publicKey !== "string" ||
          !invitation.publicKey ||
          typeof invitation.origin !== "string" ||
          serveOrigin(invitation.origin) !== origin
        )
          throw new Error("The pairing invitation is invalid.");
        const serverId = invitation.serverId,
          inviteId = invitation.inviteId;
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
            inviteId,
            token: invitation.token,
            clientId,
            label: "Studio UI",
            kind: "ui",
            publicKey,
            requestId,
          });
          const value: Draft = {
            origin,
            serverId,
            clientId,
            inviteId,
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
            body: draft.body ?? null,
            headers: { "Content-Type": "application/json" },
            signal: AbortSignal.timeout(15000),
          }),
          requestId,
        );
        const value: components["schemas"]["DevicePairResponse"] & {
          error?: string;
          code?: string;
        } = await response.json();
        if (!response.ok) {
          const error: NodeJS.ErrnoException = new Error(
            value.error ||
              `Pairing failed (${response.status}). Retry the same invitation.`,
          );
          if (response.status === 403 && value.code === "invite_unavailable") {
            await change((state) => {
              const current = state.drafts[requestId];
              if (!current) throw error;
              current.rejected = true;
            });
            error.code = "invite_unavailable";
          }
          throw error;
        }
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
          label:
            value.label ||
            (typeof invitation.label === "string" ? invitation.label : "") ||
            new URL(origin).hostname,
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
      })().finally(() => pairing.delete(inputValue.requestId));
      pairing.set(inputValue.requestId, { identity, promise });
      return promise;
    },
    async owns(serverId: string) {
      return !!(await read()).servers[serverId];
    },
    async request(
      {
        serverId,
        credentialId,
        url,
        method = "GET",
        headers = [],
        body,
        requestId,
      }: ServerRequest,
      signal?: AbortSignal,
    ) {
      const credential = (await read()).servers[serverId];
      if (!credential || credential.clientId !== credentialId)
        throw new Error("The server key is unavailable. Pair this UI again.");
      if (!["GET", "POST", "HEAD"].includes(method))
        throw new Error("Unsupported server method.");
      // The request ID is part of the signed bytes and of its header. A read
      // without one (the shell summary) gets a new ID; a write needs the
      // caller's ID, so that a retry repeats the same operation.
      if (requestId === undefined || requestId === null) {
        if (method === "POST")
          throw new Error("A server write needs a request ID.");
        requestId = crypto.randomUUID();
      }
      return signed(
        credential,
        new Request(url, {
          method,
          headers,
          ...(body?.byteLength ? { body } : {}),
          ...(signal ? { signal } : {}),
        }),
        requestId,
      );
    },
    async forget(serverId: string) {
      await change((state) => {
        delete state.servers[serverId];
        for (const [id, row] of Object.entries(state.drafts))
          if (row.serverId === serverId) delete state.drafts[id];
      });
    },
  };
}
export { createServerCredentials, canonicalRequest, serveOrigin };
