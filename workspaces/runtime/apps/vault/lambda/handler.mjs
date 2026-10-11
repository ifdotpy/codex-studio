// The vault function (docs/vault.md, "Keys" and "AWS"). It has no user database: a caller proves
// that it holds the vault identity by decrypting a challenge, and gets short presigned URLs.
import {
  DeleteObjectCommand,
  GetObjectCommand,
  ListObjectsV2Command,
  PutObjectCommand,
  S3Client,
} from "@aws-sdk/client-s3";
import { getSignedUrl } from "@aws-sdk/s3-request-presigner";
import * as age from "age-encryption";
import { createHmac, timingSafeEqual } from "node:crypto";

const TOKEN_LIFE = 10 * 60 * 1000;
const SLOT_NAME = /^passkey-[A-Za-z0-9_-]{1,256}$/;

const reply = (statusCode, body) => ({
  statusCode,
  headers: { "content-type": "application/json", "cache-control": "no-store" },
  body: JSON.stringify(body),
});

export function makeHandler({
  s3,
  env,
  now = () => Date.now(),
  presign = getSignedUrl,
}) {
  const { BUCKET, PREFIX, RECIPIENT, TOKEN_SECRET } = env;
  const mac = (expiry) =>
    createHmac("sha256", TOKEN_SECRET)
      .update(`${PREFIX}:${expiry}`)
      .digest("hex");

  function valid(token) {
    const [expiry, given] = String(token ?? "").split(".");
    const left = Number(expiry) - now();
    if (!given || !(left > 0 && left <= TOKEN_LIFE)) return false;
    const want = Buffer.from(mac(expiry), "hex");
    const got = Buffer.from(given, "hex");
    return got.length === want.length && timingSafeEqual(got, want);
  }

  async function list(prefix) {
    const keys = [];
    let token;
    do {
      const page = await s3.send(
        new ListObjectsV2Command({
          Bucket: BUCKET,
          Prefix: prefix,
          ContinuationToken: token,
        }),
      );
      keys.push(...(page.Contents ?? []).map((o) => o.Key));
      token = page.NextContinuationToken;
    } while (token);
    return keys;
  }

  return async (event) => {
    const method = event.requestContext?.http?.method;
    const path = event.rawPath;
    let body = {};
    if (event.body) {
      try {
        body = JSON.parse(
          event.isBase64Encoded
            ? Buffer.from(event.body, "base64").toString()
            : event.body,
        );
      } catch {
        return reply(400, { error: "the body is not JSON" });
      }
    }
    if (method === "GET" && path === "/slots") {
      const slots = [];
      for (const key of await list(`${PREFIX}/slots/`)) {
        const object = await s3.send(
          new GetObjectCommand({ Bucket: BUCKET, Key: key }),
        );
        const data = Buffer.from(
          await object.Body.transformToByteArray(),
        ).toString("base64");
        slots.push({ name: key.slice(PREFIX.length + 7, -4), data });
      }
      return reply(200, { prefix: PREFIX, slots });
    }
    if (method === "GET" && path === "/challenge") {
      const expiry = String(now() + TOKEN_LIFE);
      const encrypter = new age.Encrypter();
      encrypter.addRecipient(RECIPIENT);
      const challenge = await encrypter.encrypt(`${expiry}.${mac(expiry)}`);
      return reply(200, {
        challenge: Buffer.from(challenge).toString("base64"),
      });
    }
    if (!valid(body.token))
      return reply(401, { error: "the token is missing, wrong or expired" });
    if (method === "POST" && path === "/presign") {
      const result = {};
      if (body.list) result.keys = await list(`${PREFIX}/states/`);
      const keys = Array.isArray(body.keys) ? body.keys : [];
      if (
        keys.length > 1000 ||
        keys.some(
          (k) =>
            typeof k !== "string" ||
            !k.startsWith(`${PREFIX}/states/`) ||
            k.includes(".."),
        )
      ) {
        return reply(400, {
          error: "the keys are not state objects of this vault",
        });
      }
      result.urls = Object.fromEntries(
        await Promise.all(
          keys.map(async (key) => [
            key,
            await presign(
              s3,
              new GetObjectCommand({ Bucket: BUCKET, Key: key }),
              { expiresIn: 300 },
            ),
          ]),
        ),
      );
      return reply(200, result);
    }
    if (path === "/slot" && (method === "PUT" || method === "DELETE")) {
      if (!SLOT_NAME.test(body.name ?? ""))
        return reply(400, { error: "only passkey slots change here" });
      const Key = `${PREFIX}/slots/${body.name}.age`;
      if (method === "DELETE") {
        await s3.send(new DeleteObjectCommand({ Bucket: BUCKET, Key }));
        return reply(200, { deleted: body.name });
      }
      const data = Buffer.from(String(body.data ?? ""), "base64");
      if (!data.length || data.length > 4096)
        return reply(400, { error: "a slot is 1 to 4096 bytes" });
      await s3.send(
        new PutObjectCommand({
          Bucket: BUCKET,
          Key,
          Body: data,
          IfNoneMatch: "*",
        }),
      );
      return reply(200, { added: body.name });
    }
    return reply(404, { error: "no such route" });
  };
}

export const handler = makeHandler({ s3: new S3Client({}), env: process.env });
