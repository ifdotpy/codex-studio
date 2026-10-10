import {
  createPrivateKey,
  createPublicKey,
  generateKeyPairSync,
  sign,
  verify,
} from "node:crypto";
import { createInterface } from "node:readline";

// The process owns no application state. Each bounded line is one operation.
const publicKeys = new Map();
const privateKeys = new Map();
function key(cache, pem, create) {
  if (typeof pem !== "string" || pem.length > 4096)
    throw new Error("Invalid key");
  if (!cache.has(pem)) {
    if (cache.size >= 256) cache.delete(cache.keys().next().value);
    cache.set(pem, create(pem));
  }
  return cache.get(pem);
}
for await (const line of createInterface({
  input: process.stdin,
  crlfDelay: Infinity,
})) {
  if (line.length > 1024 * 1024) process.exit(1);
  let request;
  try {
    request = JSON.parse(line);
    let result;
    const data = Buffer.from(request.data ?? "", "base64");
    if (data.length > 512 * 1024) throw new Error("Invalid data");
    if (request.operation === "generate") {
      const pair = generateKeyPairSync("ed25519");
      result = {
        privateKey: pair.privateKey.export({ type: "pkcs8", format: "pem" }),
        publicKey: pair.publicKey.export({ type: "spki", format: "pem" }),
      };
    } else if (request.operation === "verify") {
      const signature = Buffer.from(request.signature, "base64");
      if (signature.length !== 64) throw new Error("Invalid signature");
      result = {
        valid: verify(
          null,
          data,
          key(publicKeys, request.publicKey, createPublicKey),
          signature,
        ),
      };
    } else if (request.operation === "sign") {
      result = {
        signature: sign(
          null,
          data,
          key(privateKeys, request.privateKey, createPrivateKey),
        ).toString("base64"),
      };
    } else {
      throw new Error("Invalid operation");
    }
    process.stdout.write(JSON.stringify({ id: request.id, result }) + "\n");
  } catch {
    process.stdout.write(
      JSON.stringify({ id: request?.id, failure: true }) + "\n",
    );
  }
}
