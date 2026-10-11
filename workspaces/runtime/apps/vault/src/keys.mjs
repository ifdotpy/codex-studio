// Vault keys and slots (docs/vault.md, "Keys"). A slot is the vault identity encrypted with age to
// the recipient of one opener: the paper key, or a passkey whose PRF output is an X25519 secret.
import * as age from "age-encryption";
import {
  entropyToMnemonic,
  mnemonicToEntropy,
  validateMnemonic,
} from "@scure/bip39";
import { wordlist } from "@scure/bip39/wordlists/english.js";

const CHARSET = "qpzry9x8gf2tvdw0s3jn54khce6mua7l";
const GENERATOR = [0x3b6a57b2, 0x26508e6d, 0x1ea119fa, 0x3d4233dd, 0x2a1462b3];

/** The PRF input of every vault passkey. */
export const PRF_SALT = new TextEncoder().encode("layr-vault/v1");

function polymod(values) {
  let check = 1;
  for (const value of values) {
    const top = check >>> 25;
    check = ((check & 0x1ffffff) << 5) ^ value;
    for (let i = 0; i < 5; i++) if ((top >>> i) & 1) check ^= GENERATOR[i];
  }
  return check;
}

function expand(hrp) {
  return [
    ...[...hrp].map((c) => c.charCodeAt(0) >> 5),
    0,
    ...[...hrp].map((c) => c.charCodeAt(0) & 31),
  ];
}

function convert(data, from, to, pad) {
  let accumulator = 0;
  let bits = 0;
  const out = [];
  for (const value of data) {
    accumulator = (accumulator << from) | value;
    bits += from;
    while (bits >= to) {
      bits -= to;
      out.push((accumulator >> bits) & ((1 << to) - 1));
    }
  }
  if (pad && bits > 0) out.push((accumulator << (to - bits)) & ((1 << to) - 1));
  else if (
    !pad &&
    (bits >= from || (accumulator << (to - bits)) & ((1 << to) - 1))
  )
    throw new Error("bad padding");
  return out;
}

/** Bech32 (BIP 173) without the 90-character limit, as age uses it. */
export function bech32Encode(hrp, bytes) {
  const data = convert(bytes, 8, 5, true);
  const check = polymod([...expand(hrp), ...data, 0, 0, 0, 0, 0, 0]) ^ 1;
  const sum = Array.from(
    { length: 6 },
    (_, i) => (check >>> (5 * (5 - i))) & 31,
  );
  return hrp + "1" + [...data, ...sum].map((v) => CHARSET[v]).join("");
}

export function bech32Decode(text) {
  const lower = text.toLowerCase();
  const split = lower.lastIndexOf("1");
  const hrp = lower.slice(0, split);
  const values = Array.from(lower.slice(split + 1), (c) => {
    const v = CHARSET.indexOf(c);
    if (v < 0) throw new Error("not bech32");
    return v;
  });
  if (polymod([...expand(hrp), ...values]) !== 1)
    throw new Error("bad bech32 checksum");
  return {
    hrp,
    bytes: Uint8Array.from(convert(values.slice(0, -6), 5, 8, false)),
  };
}

/** The age identity whose X25519 secret is these 32 bytes (a passkey PRF output). */
export function identityFromSecret(secret) {
  if (secret.length !== 32) throw new Error("an X25519 secret has 32 bytes");
  return bech32Encode("age-secret-key-", secret).toUpperCase();
}

export const generateIdentity = () => age.generateIdentity();
export const recipientOf = (identity) => age.identityToRecipient(identity);

/** Encrypt the vault identity to one opener. */
export async function sealSlot(vaultIdentity, openerRecipient) {
  const encrypter = new age.Encrypter();
  encrypter.addRecipient(openerRecipient);
  return encrypter.encrypt(vaultIdentity + "\n");
}

/** The vault identity in a slot, opened with the opener's identity. */
export async function openSlot(slot, openerIdentity) {
  const decrypter = new age.Decrypter();
  decrypter.addIdentity(openerIdentity);
  const text = new TextDecoder().decode(await decrypter.decrypt(slot)).trim();
  if (!/^AGE-SECRET-KEY-1[0-9A-Z]+$/.test(text))
    throw new Error("the slot holds no vault identity");
  return text;
}

/** Bytes encrypted to an identity, decrypted (the function's challenge). */
export async function decryptWith(identity, data) {
  const decrypter = new age.Decrypter();
  decrypter.addIdentity(identity);
  return decrypter.decrypt(data);
}

/** The paper key as 24 BIP39 words: the 32-byte X25519 secret of the paper identity. */
export function wordsOfIdentity(identity) {
  return entropyToMnemonic(bech32Decode(identity).bytes, wordlist);
}

/**
 * The age identity on paper: 24 BIP39 words (numbers between them and the first four letters
 * of a word are fine, as BIP39 allows) or the AGE-SECRET-KEY-1… form.
 */
export function identityFromPaper(text) {
  const compact = text.replace(/\s+/g, "");
  if (/^AGE-SECRET-KEY-1/i.test(compact)) return compact.toUpperCase();
  const words = text
    .toLowerCase()
    .split(/[\s,;]+/)
    .filter((w) => w && !/^\d+[.)]?$/.test(w))
    .map((w) => {
      if (wordlist.includes(w)) return w;
      const found =
        w.length >= 4
          ? wordlist.filter((x) => x.startsWith(w.slice(0, 4)))
          : [];
      if (found.length !== 1)
        throw new Error(`"${w}" is not a word of the paper key list`);
      return found[0];
    });
  const mnemonic = words.join(" ");
  if (words.length !== 24 || !validateMnemonic(mnemonic, wordlist)) {
    throw new Error(
      "The paper key is 24 words; these words or their order do not match (checksum).",
    );
  }
  return identityFromSecret(mnemonicToEntropy(mnemonic, wordlist));
}
