import { publicKeyPem } from "./pairing";
import {
  browserDatabases,
  browserIndexedDB,
  browserStores,
} from "../browserPersistence";

/**
 * The one key of this device (a browser profile or a desktop window). It signs for every
 * paired server. Its id is derived from the public key, so a server that learns the key
 * from another server computes the same id (docs/decisions/
 * 2026-10-28-target-client-server-architecture.md, "Device identity").
 */
export type DeviceKey = {
  slot: "device";
  id: string;
  privateKey: CryptoKey;
  publicKey: string;
};

const SLOT = "device";
const STORE = browserStores.deviceKey;

/** `dev-` and the SHA-256 of the SPKI public key in hex: 68 characters. */
export async function deviceIdOf(spki: ArrayBuffer): Promise<string> {
  const hash = new Uint8Array(await crypto.subtle.digest("SHA-256", spki));
  return `dev-${Array.from(hash, (byte) => byte.toString(16).padStart(2, "0")).join("")}`;
}

/** A new non-extractable Ed25519 key. Nothing is stored. */
export async function createDeviceKey(): Promise<DeviceKey> {
  const keys = await crypto.subtle.generateKey({ name: "Ed25519" }, false, [
    "sign",
    "verify",
  ]);
  const spki = await crypto.subtle.exportKey("spki", keys.publicKey);
  return {
    slot: SLOT,
    id: await deviceIdOf(spki),
    privateKey: keys.privateKey,
    publicKey: publicKeyPem(spki),
  };
}

function database() {
  return new Promise<IDBDatabase>((resolve, reject) => {
    const request = browserIndexedDB()!.open(browserDatabases.deviceKey, 1);
    request.onupgradeneeded = () =>
      request.result.createObjectStore(STORE, { keyPath: "slot" });
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error);
  });
}

async function run<T>(
  mode: IDBTransactionMode,
  action: (store: IDBObjectStore) => IDBRequest<T>,
): Promise<T> {
  const db = await database();
  return new Promise((resolve, reject) => {
    const tx = db.transaction(
      STORE,
      mode,
      mode === "readwrite" ? { durability: "strict" } : undefined,
    );
    const result = action(tx.objectStore(STORE));
    tx.oncomplete = () => {
      db.close();
      resolve(result.result);
    };
    tx.onabort = tx.onerror = () => {
      db.close();
      reject(
        tx.error || result.error || new Error("The device key is unavailable."),
      );
    };
  });
}

let current: Promise<DeviceKey> | undefined;

async function load(): Promise<DeviceKey> {
  const saved = await run<DeviceKey | undefined>("readonly", (store) =>
    store.get(SLOT),
  );
  if (saved) return saved;
  const created = await createDeviceKey();
  try {
    await run("readwrite", (store) => store.add(created));
    return created;
  } catch (error) {
    // Another window of this profile created the key first. Its key is the device key.
    const winner = await run<DeviceKey | undefined>("readonly", (store) =>
      store.get(SLOT),
    );
    if (winner) return winner;
    throw error;
  }
}

/** The key of this device. It is created on first use and then never changes. */
export function deviceKey(): Promise<DeviceKey> {
  current ??= load().catch((error) => {
    current = undefined;
    throw error;
  });
  return current;
}

/**
 * Replace the key after a server revoked it. A revocation is final, so the device pairs
 * again with a new key and gets a new id.
 */
export async function replaceDeviceKey(): Promise<DeviceKey> {
  const created = await createDeviceKey();
  await run("readwrite", (store) => store.put(created));
  current = Promise.resolve(created);
  return created;
}
