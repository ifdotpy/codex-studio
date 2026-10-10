import type { ServerCredentialAdapter, PairAttemptIdentity } from "./transport";
import {
  isRemoteServerView,
  serverParentOrigin,
  serverViewId,
} from "./environment";
import {
  parseInvitation,
  pairedServer,
  publicKeyPem,
  signedRequest,
  requestIdentity,
  type PairReceipt,
} from "./pairing";
import type { StudioServer } from "./registry";
type Credential = {
  inviteId?: string;
  id: string;
  origin: string;
  serverId: string;
  clientId: string;
  privateKey: CryptoKey;
  publicKey: string;
  body?: string;
  requestId?: string;
  targetPublicKey?: string;
  receipt?: StudioServer;
};
function database() {
  return new Promise<IDBDatabase>((resolve, reject) => {
    const request = indexedDB.open("studio-server-credentials-v1", 1);
    request.onupgradeneeded = () =>
      request.result.createObjectStore("credentials", { keyPath: "id" });
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error);
  });
}
async function record<T>(
  mode: IDBTransactionMode,
  action: (store: IDBObjectStore) => IDBRequest<T>,
): Promise<T> {
  const db = await database();
  return new Promise((resolve, reject) => {
    const tx = db.transaction(
      "credentials",
      mode,
      mode === "readwrite" ? { durability: "strict" } : undefined,
    );
    const result = action(tx.objectStore("credentials"));
    tx.oncomplete = () => {
      db.close();
      resolve(result.result);
    };
    tx.onabort = tx.onerror = () => {
      db.close();
      reject(
        tx.error ||
          result.error ||
          new Error("The server key could not be saved."),
      );
    };
  });
}
export class BrowserServerCredentials implements ServerCredentialAdapter {
  async frameSigningCredential(server: StudioServer) {
    const credential = await record<Credential | undefined>(
      "readonly",
      (store) => store.get(server.credentialId!),
    );
    if (
      !credential ||
      credential.origin !== server.origin ||
      credential.serverId !== server.id ||
      credential.clientId !== server.credentialId
    )
      throw new Error("The server key is unavailable. Pair this UI again.");
    return { clientId: credential.clientId, privateKey: credential.privateKey };
  }
  async hasPairAttempt(attempt: PairAttemptIdentity) {
    const draft = await record<Credential | undefined>("readonly", (store) =>
      store.get(`pair:${attempt.requestId}`),
    );
    if (!draft) return false;
    if (
      draft.serverId !== attempt.serverId ||
      draft.origin !== attempt.origin ||
      (attempt.inviteId && draft.inviteId !== attempt.inviteId)
    )
      throw new Error(
        "The saved credential belongs to another pairing attempt.",
      );
    return !!(draft.body || draft.receipt);
  }
  async pair(origin: string, code: string, requestId: string) {
    const invitation = parseInvitation(code, origin);
    const pendingId = `pair:${requestId}`;
    let draft = await record<Credential | undefined>("readonly", (store) =>
      store.get(pendingId),
    );
    if (
      draft &&
      (draft.origin !== origin ||
        draft.serverId !== invitation.serverId ||
        draft.inviteId !== invitation.inviteId)
    )
      throw new Error("This pairing request belongs to another server.");
    if (draft?.receipt) return draft.receipt;
    if (!draft) {
      const keys = await crypto.subtle.generateKey({ name: "Ed25519" }, false, [
        "sign",
        "verify",
      ]);
      const publicKey = publicKeyPem(
        await crypto.subtle.exportKey("spki", keys.publicKey),
      );
      const clientId = crypto.randomUUID();
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
      draft = {
        id: pendingId,
        inviteId: invitation.inviteId,
        origin,
        serverId: invitation.serverId,
        clientId,
        privateKey: keys.privateKey,
        publicKey,
        body,
        requestId,
      };
      await record("readwrite", (store) => store.add(draft!));
    }
    const request = new Request(origin + "/api/multi-server/v1/pair", {
      method: "POST",
      body: draft.body,
      headers: { "Content-Type": "application/json" },
      signal: AbortSignal.timeout(15000),
    });
    const response = await fetch(
      await signedRequest(
        request,
        draft.serverId,
        draft.clientId,
        draft.privateKey,
        requestId,
      ),
    );
    const receipt = (await response.json()) as PairReceipt & { error?: string };
    if (!response.ok)
      throw new Error(
        receipt.error ||
          `Pairing failed (${response.status}). Retry the same invitation.`,
      );
    const server = pairedServer(receipt, invitation, draft.clientId);
    const { body: _body, requestId: _requestId, ...credential } = draft;
    await record("readwrite", (store) =>
      store.put({
        ...credential,
        targetPublicKey: receipt.publicKey,
        id: draft!.clientId,
      }),
    );
    await record("readwrite", (store) =>
      store.put({
        id: pendingId,
        serverId: draft!.serverId,
        origin: draft!.origin,
        receipt: server,
        inviteId: invitation.inviteId,
      }),
    );
    return server;
  }
  async fetch(server: StudioServer, request: Request) {
    const url = new URL(request.url);
    if (
      url.origin !== server.origin ||
      !url.pathname.startsWith("/api/") ||
      !["GET", "POST", "HEAD"].includes(request.method)
    )
      throw new Error("The request does not belong to this server.");
    if (isRemoteServerView && serverViewId === server.id) {
      const { clientId, privateKey } =
        await requestFrameSigningCredential(server);
      return fetch(
        await signedRequest(
          request,
          server.id,
          clientId,
          privateKey,
          await requestIdentity(request),
        ),
      );
    }
    const credential = await record<Credential | undefined>(
      "readonly",
      (store) => store.get(server.credentialId!),
    );
    if (
      !credential ||
      credential.origin !== server.origin ||
      credential.serverId !== server.id ||
      credential.clientId !== server.credentialId
    )
      throw new Error("The server key is unavailable. Pair this UI again.");
    return fetch(
      await signedRequest(
        request,
        server.id,
        credential.clientId,
        credential.privateKey,
        await requestIdentity(request),
      ),
    );
  }
  async forget(server: StudioServer) {
    const rows = await record<Credential[]>("readonly", (store) =>
      store.getAll(),
    );
    await record("readwrite", (store) => {
      for (const row of rows)
        if (row.serverId === server.id) store.delete(row.id);
      return store.delete(server.credentialId!);
    });
  }
}

const frameCredentials = new Map<
  string,
  Promise<{ clientId: string; privateKey: CryptoKey }>
>();
function requestFrameSigningCredential(server: StudioServer) {
  const current = frameCredentials.get(server.id);
  if (current) return current;
  const pending = new Promise<{ clientId: string; privateKey: CryptoKey }>(
    (resolve, reject) => {
      const correlation = crypto.randomUUID();
      const timer = setTimeout(
        () => finish(new Error("The server key request timed out.")),
        5000,
      );
      const finish = (
        error?: Error,
        value?: { clientId: string; privateKey: CryptoKey },
      ) => {
        clearTimeout(timer);
        window.removeEventListener("message", receive);
        if (error) {
          frameCredentials.delete(server.id);
          reject(error);
        } else if (value) resolve(value);
      };
      const receive = (event: MessageEvent) => {
        if (
          event.source !== window.parent ||
          event.origin !== serverParentOrigin ||
          event.data?.kind !== "studio-server-frame-credential-result" ||
          event.data?.serverId !== server.id ||
          event.data?.correlation !== correlation
        )
          return;
        if (typeof event.data.error === "string") {
          finish(new Error(event.data.error));
          return;
        }
        const value = event.data.credential;
        if (
          !value ||
          value.serverId !== server.id ||
          value.clientId !== server.credentialId ||
          !value.privateKey ||
          value.privateKey.type !== "private" ||
          value.privateKey.extractable
        ) {
          finish(new Error("The server key response is invalid."));
          return;
        }
        finish(undefined, value);
      };
      window.addEventListener("message", receive);
      window.parent.postMessage(
        {
          kind: "studio-server-frame-credential-request",
          serverId: server.id,
          correlation,
        },
        serverParentOrigin,
      );
    },
  );
  frameCredentials.set(server.id, pending);
  return pending;
}
