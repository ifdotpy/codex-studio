#!/usr/bin/env node
import { generateKeyPairSync, sign, verify } from "node:crypto";

const input = await new Promise((resolve, reject) => {
  const chunks = [];
  let size = 0;
  process.stdin.on("data", (chunk) => {
    size += chunk.length;
    if (size > 1024 * 1024) {
      reject(new Error("crypto request too large"));
      process.stdin.destroy();
      return;
    }
    chunks.push(chunk);
  });
  process.stdin.on("end", () => resolve(Buffer.concat(chunks)));
  process.stdin.on("error", reject);
});

const request = JSON.parse(input.toString("utf8"));
let result;
if (request.operation === "generate") {
  const pair = generateKeyPairSync("ed25519");
  result = {
    privateKey: pair.privateKey.export({ type: "pkcs8", format: "pem" }),
    publicKey: pair.publicKey.export({ type: "spki", format: "pem" }),
  };
} else if (request.operation === "sign") {
  const data = Buffer.from(request.data, "base64");
  const privateKey = request.privateKey;
  if (data.length > 512 * 1024 || typeof privateKey !== "string")
    throw new Error("invalid signing request");
  result = { signature: sign(null, data, privateKey).toString("base64") };
} else if (request.operation === "verify") {
  const data = Buffer.from(request.data, "base64");
  const signature = Buffer.from(request.signature, "base64");
  if (data.length > 512 * 1024 || signature.length !== 64)
    throw new Error("invalid verification request");
  result = {
    valid: verify(null, data, request.publicKey, signature),
  };
} else {
  throw new Error("unknown crypto operation");
}
process.stdout.write(JSON.stringify(result));
