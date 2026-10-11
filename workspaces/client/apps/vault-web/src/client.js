// The vault function, the passkeys and the reader of the site (docs/vault.md).
import { VaultReader, zstdReady } from "studio-vault/reader";
import {
  PRF_SALT,
  decryptWith,
  identityFromPaper,
  identityFromSecret,
  openSlot,
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

export class Vault {
  static async open() {
    const config = await (
      await fetch("./config.json", { cache: "no-store" })
    ).json();
    await zstdReady("./zstd.wasm");
    return new Vault(config);
  }

  constructor(config) {
    this.base = config.functionUrl.replace(/\/?$/, "/");
    this.prefix = config.prefix;
    this.identity = null;
    this.token = null;
    this.urls = new Map();
  }

  async call(method, path, body) {
    const init = body
      ? {
          method,
          headers: { "content-type": "application/json" },
          body: JSON.stringify(body),
        }
      : { method };
    const response = await fetch(this.base + path, init);
    const data = await response.json();
    if (!response.ok)
      throw Object.assign(new Error(data.error ?? `HTTP ${response.status}`), {
        status: response.status,
      });
    return data;
  }

  async slots() {
    return (await this.call("GET", "slots")).slots.map((s) => ({
      name: s.name,
      data: unb64(s.data),
    }));
  }

  /** Call with a fresh token after a 401 (tokens live 10 minutes). */
  async authorized(method, path, body) {
    for (let attempt = 0; attempt < 2; attempt++) {
      if (!this.token) {
        const { challenge } = await this.call("GET", "challenge");
        this.token = new TextDecoder().decode(
          await decryptWith(this.identity, unb64(challenge)),
        );
      }
      try {
        return await this.call(method, path, { ...body, token: this.token });
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
    if (!passkeys.length)
      throw new Error("No passkey is set up for this vault yet.");
    const evalByCredential = Object.fromEntries(
      passkeys.map((s) => [s.name.slice(8), { first: PRF_SALT }]),
    );
    const credential = await navigator.credentials.get({
      publicKey: {
        challenge: crypto.getRandomValues(new Uint8Array(32)),
        rpId: location.hostname,
        userVerification: "required",
        allowCredentials: passkeys.map((s) => ({
          type: "public-key",
          id: unb64url(s.name.slice(8)),
        })),
        extensions: { prf: { evalByCredential } },
      },
    });
    const secret = credential.getClientExtensionResults().prf?.results?.first;
    if (!secret)
      throw new Error(
        "This passkey provider gave no PRF result. Use the paper key, or a provider with PRF.",
      );
    const slot = passkeys.find(
      (s) => s.name === `passkey-${b64url(credential.rawId)}`,
    );
    await this.unlocked(
      await openSlot(slot.data, identityFromSecret(new Uint8Array(secret))),
    );
  }

  /** Make a passkey on this device and give it a slot. */
  async addPasskey(label) {
    const credential = await navigator.credentials.create({
      publicKey: {
        challenge: crypto.getRandomValues(new Uint8Array(32)),
        rp: { id: location.hostname, name: `Vault ${this.prefix}` },
        user: {
          id: crypto.getRandomValues(new Uint8Array(16)),
          name: `vault-${this.prefix}`,
          displayName: label || `Vault ${this.prefix}`,
        },
        pubKeyCredParams: [
          { type: "public-key", alg: -7 },
          { type: "public-key", alg: -257 },
        ],
        authenticatorSelection: {
          residentKey: "required",
          userVerification: "required",
        },
        extensions: { prf: { eval: { first: PRF_SALT } } },
      },
    });
    let secret = credential.getClientExtensionResults().prf?.results?.first;
    if (!secret) {
      // Some providers give PRF results only when a passkey signs in.
      const again = await navigator.credentials.get({
        publicKey: {
          challenge: crypto.getRandomValues(new Uint8Array(32)),
          rpId: location.hostname,
          userVerification: "required",
          allowCredentials: [{ type: "public-key", id: credential.rawId }],
          extensions: { prf: { eval: { first: PRF_SALT } } },
        },
      });
      secret = again.getClientExtensionResults().prf?.results?.first;
    }
    if (!secret)
      throw new Error(
        "This passkey provider has no PRF, so it cannot open the vault.",
      );
    const opener = identityFromSecret(new Uint8Array(secret));
    const slot = await sealSlot(this.identity, await recipientOf(opener));
    const name = `passkey-${b64url(credential.rawId)}`;
    await this.authorized("PUT", "slot", { name, data: b64(slot) });
    return name;
  }

  async removePasskey(name) {
    await this.authorized("DELETE", "slot", { name });
  }

  async unlocked(identity) {
    this.identity = identity;
    this.token = null;
    const { keys } = await this.authorized("POST", "presign", { list: true });
    this.reader = new VaultReader({
      prefix: this.prefix,
      keys,
      identity,
      get: (key) => this.fetchObject(key),
    });
  }

  async fetchObject(key) {
    const known = this.urls.get(key);
    let url = known && known.until > Date.now() ? known.url : null;
    if (!url) {
      url = (await this.authorized("POST", "presign", { keys: [key] })).urls[
        key
      ];
      this.urls.set(key, { url, until: Date.now() + 240_000 });
    }
    const response = await fetch(url);
    if (!response.ok)
      throw new Error(`Reading ${key}: HTTP ${response.status}`);
    return new Uint8Array(await response.arrayBuffer());
  }

  lock() {
    this.identity = null;
    this.token = null;
    this.reader = null;
    this.urls.clear();
  }
}
