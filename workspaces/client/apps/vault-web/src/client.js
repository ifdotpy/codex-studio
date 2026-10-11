// The gateway API, the passkeys and the reader of the site (docs/vault.md, "Gateway").
import { VaultReader, zstdReady } from "studio-vault/reader";
import {
  decryptWith,
  identityFromPaper,
  identityFromSecret,
  openSlot,
  prfSalt,
  recipientOf,
  sealSlot,
} from "studio-vault/keys";

const b64 = (bytes) => btoa(String.fromCharCode(...new Uint8Array(bytes)));
const unb64 = (text) => Uint8Array.from(atob(text), (c) => c.charCodeAt(0));
export const b64url = (bytes) =>
  b64(bytes).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
const unb64url = (text) =>
  unb64(
    text.replace(/-/g, "+").replace(/_/g, "/") +
      "===".slice((text.length + 3) % 4),
  );

async function request(url, { method = "GET", body, token, raw } = {}) {
  const headers = {};
  if (body) headers["content-type"] = "application/json";
  if (token) headers.authorization = `Bearer ${token}`;
  const response = await fetch(url, {
    method,
    headers,
    cache: "no-store",
    ...(body ? { body: JSON.stringify(body) } : {}),
  });
  if (!response.ok) {
    const data = await response.json().catch(() => ({}));
    throw Object.assign(new Error(data.error ?? `HTTP ${response.status}`), {
      status: response.status,
    });
  }
  return raw ? response : response.json();
}

/** The vaults that this gateway serves. */
export async function projects() {
  return (await request("/api/projects")).projects;
}

export class Vault {
  static async open(prefix) {
    await zstdReady("/zstd.wasm");
    return new Vault(prefix);
  }

  constructor(prefix) {
    this.prefix = prefix;
    this.base = `/api/p/${encodeURIComponent(prefix)}/`;
    this.salt = prfSalt(prefix);
    this.identity = null;
    this.token = null;
  }

  async slots() {
    return (await request(`${this.base}slots`)).slots.map((s) => ({
      name: s.name,
      data: unb64(s.data),
    }));
  }

  /** Call with a fresh token after a 401 (tokens live 10 minutes). */
  async authorized(path, options = {}) {
    for (let attempt = 0; ; attempt++) {
      if (!this.token) {
        const { challenge } = await request(`${this.base}challenge`);
        this.token = new TextDecoder().decode(
          await decryptWith(this.identity, unb64(challenge)),
        );
      }
      try {
        return await request(this.base + path, {
          ...options,
          token: this.token,
        });
      } catch (error) {
        if (error.status !== 401 || attempt) throw error;
        this.token = null;
      }
    }
  }

  async unlockWithPaper(paperKey, slots) {
    const paper = slots.find((s) => s.name === "paper");
    if (!paper) throw new Error("This vault has no paper slot.");
    await this.unlocked(
      await openSlot(paper.data, identityFromPaper(paperKey)),
    );
  }

  async unlockWithPasskey(slots) {
    const passkeys = slots.filter((s) => s.name.startsWith("passkey-"));
    if (!passkeys.length) throw new Error("No passkey opens this vault yet.");
    const credential = await navigator.credentials.get({
      publicKey: {
        challenge: crypto.getRandomValues(new Uint8Array(32)),
        rpId: location.hostname,
        userVerification: "required",
        allowCredentials: passkeys.map((s) => ({
          type: "public-key",
          id: unb64url(s.name.slice(8)),
        })),
        extensions: { prf: { eval: { first: this.salt } } },
      },
    });
    const slot = passkeys.find(
      (s) => s.name === `passkey-${b64url(credential.rawId)}`,
    );
    await this.unlocked(await openSlot(slot.data, this.opener(credential)));
  }

  opener(credential) {
    const secret = credential.getClientExtensionResults().prf?.results?.first;
    if (!secret)
      throw new Error(
        "This passkey provider gave no PRF result, so it cannot open the vault.",
      );
    return identityFromSecret(new Uint8Array(secret));
  }

  /** Make a new passkey on this device and give it a slot. */
  async addPasskey(label) {
    const created = await navigator.credentials.create({
      publicKey: {
        challenge: crypto.getRandomValues(new Uint8Array(32)),
        rp: { id: location.hostname, name: location.hostname },
        user: {
          id: crypto.getRandomValues(new Uint8Array(16)),
          name: `vault@${location.hostname}`,
          displayName: label || "Vault",
        },
        pubKeyCredParams: [
          { type: "public-key", alg: -7 },
          { type: "public-key", alg: -257 },
        ],
        authenticatorSelection: {
          residentKey: "required",
          userVerification: "required",
        },
        extensions: { prf: { eval: { first: this.salt } } },
      },
    });
    // Some providers give PRF results only when a passkey signs in.
    const credential = created.getClientExtensionResults().prf?.results?.first
      ? created
      : await this.pick([created.rawId]);
    return this.seal(credential);
  }

  /** Give a slot to a passkey that already exists, such as one that opens another vault. */
  async useExistingPasskey() {
    return this.seal(await this.pick([]));
  }

  pick(ids) {
    return navigator.credentials.get({
      publicKey: {
        challenge: crypto.getRandomValues(new Uint8Array(32)),
        rpId: location.hostname,
        userVerification: "required",
        allowCredentials: ids.map((id) => ({ type: "public-key", id })),
        extensions: { prf: { eval: { first: this.salt } } },
      },
    });
  }

  async seal(credential) {
    const slot = await sealSlot(
      this.identity,
      await recipientOf(this.opener(credential)),
    );
    const name = `passkey-${b64url(credential.rawId)}`;
    await this.authorized(`slots/${name}`, {
      method: "PUT",
      body: { data: b64(slot) },
    });
    return name;
  }

  async removePasskey(name) {
    await this.authorized(`slots/${name}`, { method: "DELETE" });
  }

  async unlocked(identity) {
    this.identity = identity;
    this.token = null;
    const { states } = await this.authorized("states");
    this.reader = new VaultReader({
      prefix: this.prefix,
      keys: states.map((name) => `${this.prefix}/states/${name}`),
      identity,
      get: (key) => this.fetchObject(key),
    });
  }

  async fetchObject(key) {
    const name = key.slice(this.prefix.length + 8);
    const response = await this.authorized(`states/${name}`, { raw: true });
    return new Uint8Array(await response.arrayBuffer());
  }

  lock() {
    this.identity = null;
    this.token = null;
    this.reader = null;
  }
}
