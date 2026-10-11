// The vault reader (docs/vault.md, "Reader"): the browser, vault-restore and vault-view use it.
// The Python reference reader in vm-guest/vault_reader.py has the same logic.
import * as age from "age-encryption";
import {
  createDCtx,
  decompress,
  decompressUsingDict,
  init,
} from "@bokuweb/zstd-wasm";

let zstd;

/** Load the zstd decoder once; `wasmUrl` is needed in a browser bundle. */
export function zstdReady(wasmUrl) {
  zstd ??= wasmUrl ? init(wasmUrl) : init();
  return zstd;
}

/** One zstd frame; a delta frame takes the base content as a raw dictionary. */
export async function unzstd(frame, base) {
  await zstdReady();
  return base && base.length
    ? decompressUsingDict(createDCtx(), frame, base)
    : decompress(frame);
}

const TREE = /^(\d{10})\.([0-9a-f]{8})\.tree\.age$/;

export class VaultReader {
  /**
   * @param {{prefix: string, keys: string[], get: (key: string) => Promise<Uint8Array>, identity: string}} options
   */
  constructor({ prefix, keys, get, identity }) {
    this.prefix = prefix;
    this.get = get;
    this.identity = identity;
    this.cache = new Map();
    this.trees = new Map();
    for (const key of keys) {
      const match =
        key.startsWith(`${prefix}/states/`) &&
        TREE.exec(key.slice(prefix.length + 8));
      if (match) {
        const seq = Number(match[1]);
        this.trees.set(seq, [...(this.trees.get(seq) ?? []), match[2]].sort());
      }
    }
  }

  /** Every pushed state, oldest first: [{seq, attempt}]. */
  states() {
    return [...this.trees.keys()]
      .sort((a, b) => a - b)
      .map((seq) => ({ seq, attempt: this.trees.get(seq)[0] }));
  }

  latest() {
    const all = this.states();
    if (!all.length) throw new Error("the vault has no state yet");
    return all[all.length - 1];
  }

  key(seq, attempt, part) {
    const name =
      `${String(seq).padStart(10, "0")}.${attempt}.` +
      (part === undefined ? "tree" : `${String(part).padStart(4, "0")}.data`);
    return `${this.prefix}/states/${name}.age`;
  }

  async object(key) {
    if (!this.cache.has(key)) {
      this.cache.set(
        key,
        (async () => {
          const decrypter = new age.Decrypter();
          decrypter.addIdentity(this.identity);
          return decrypter.decrypt(await this.get(key));
        })(),
      );
    }
    return this.cache.get(key);
  }

  /** The header and the canonical tree bytes of one state. */
  async tree(seq, attempt) {
    const raw = await this.object(this.key(seq, attempt));
    const newline = raw.indexOf(10);
    const header = JSON.parse(
      new TextDecoder().decode(raw.subarray(0, newline)),
    );
    const body = raw.subarray(newline + 1);
    if (header.tree === "full")
      return { header, canonical: await unzstd(body) };
    const base = await this.tree(...header.base);
    return { header, canonical: await unzstd(body, base.canonical) };
  }

  /** {path: [mode, size, sha256, chain]} of one state (the latest by default). */
  async files(seq, attempt) {
    const state =
      seq === undefined
        ? this.latest()
        : { seq, attempt: attempt ?? this.trees.get(seq)[0] };
    const { canonical } = await this.tree(state.seq, state.attempt);
    return JSON.parse(new TextDecoder().decode(canonical)).files;
  }

  /** The content of one tree entry: its chain decoded from the full frame up. */
  async read(entry) {
    let data;
    for (const [seq, attempt, part, offset, length, kind] of [
      ...entry[3],
    ].reverse()) {
      const blob = await this.object(this.key(seq, attempt, part));
      const frame = blob.subarray(offset, offset + length);
      data = kind === "delta" ? await unzstd(frame, data) : await unzstd(frame);
    }
    return data;
  }
}
