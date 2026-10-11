// One vault as the gateway sees it (docs/vault.md, "Gateway"). It has no user database: a caller
// proves that it holds the vault identity by decrypting a challenge, and gets a short token.
import {
  DeleteObjectCommand,
  GetObjectCommand,
  ListObjectsV2Command,
  PutObjectCommand,
} from "@aws-sdk/client-s3";
import * as age from "age-encryption";
import { createHmac, timingSafeEqual } from "node:crypto";

export const TOKEN_LIFE = 10 * 60 * 1000;
export const SLOT_NAME = /^passkey-[A-Za-z0-9_-]{1,256}$/;
const STATE_NAME = /^[0-9]{10}\.[0-9a-f]{8}(\.[0-9]{4}\.data|\.tree)\.age$/;

export class VaultError extends Error {
  constructor(status, message) {
    super(message);
    this.status = status;
  }
}

/** `secret` signs tokens; `prefix` binds them to this vault. */
export function makeVault({
  s3,
  bucket,
  prefix,
  recipient,
  secret,
  now = () => Date.now(),
}) {
  const mac = (expiry) =>
    createHmac("sha256", secret).update(`${prefix}:${expiry}`).digest("hex");

  async function list(under) {
    const keys = [];
    let token;
    do {
      const page = await s3.send(
        new ListObjectsV2Command({
          Bucket: bucket,
          Prefix: under,
          ContinuationToken: token,
        }),
      );
      keys.push(...(page.Contents ?? []).map((o) => o.Key));
      token = page.NextContinuationToken;
    } while (token);
    return keys;
  }

  return {
    prefix,

    valid(token) {
      const [expiry, given] = String(token ?? "").split(".");
      const left = Number(expiry) - now();
      if (!given || !(left > 0 && left <= TOKEN_LIFE)) return false;
      const want = Buffer.from(mac(expiry), "hex");
      const got = Buffer.from(given, "hex");
      return got.length === want.length && timingSafeEqual(got, want);
    },

    async slots() {
      const slots = [];
      for (const key of await list(`${prefix}/slots/`)) {
        const object = await s3.send(
          new GetObjectCommand({ Bucket: bucket, Key: key }),
        );
        const data = Buffer.from(await object.Body.transformToByteArray());
        slots.push({
          name: key.slice(prefix.length + 7, -4),
          data: data.toString("base64"),
        });
      }
      return slots;
    },

    async challenge() {
      const expiry = String(now() + TOKEN_LIFE);
      const encrypter = new age.Encrypter();
      encrypter.addRecipient(recipient);
      const challenge = await encrypter.encrypt(`${expiry}.${mac(expiry)}`);
      return Buffer.from(challenge).toString("base64");
    },

    /** Object names below states/, without the prefix. */
    async states() {
      const under = `${prefix}/states/`;
      return (await list(under)).map((key) => key.slice(under.length));
    },

    /** The body stream and length of one state object. */
    async object(name) {
      if (!STATE_NAME.test(name))
        throw new VaultError(400, "not a state object name");
      try {
        const object = await s3.send(
          new GetObjectCommand({
            Bucket: bucket,
            Key: `${prefix}/states/${name}`,
          }),
        );
        return { body: object.Body, length: object.ContentLength };
      } catch (error) {
        if (error.name === "NoSuchKey")
          throw new VaultError(404, "no such state object");
        throw error;
      }
    },

    async addSlot(name, data) {
      if (!SLOT_NAME.test(name))
        throw new VaultError(400, "only passkey slots change here");
      const body = Buffer.from(String(data ?? ""), "base64");
      if (!body.length || body.length > 4096)
        throw new VaultError(400, "a slot is 1 to 4096 bytes");
      try {
        await s3.send(
          new PutObjectCommand({
            Bucket: bucket,
            Key: `${prefix}/slots/${name}.age`,
            Body: body,
            IfNoneMatch: "*",
          }),
        );
      } catch (error) {
        if (
          error.name === "PreconditionFailed" ||
          error.$metadata?.httpStatusCode === 412
        )
          throw new VaultError(409, "this passkey already opens the vault");
        throw error;
      }
    },

    async removeSlot(name) {
      if (!SLOT_NAME.test(name))
        throw new VaultError(400, "only passkey slots change here");
      await s3.send(
        new DeleteObjectCommand({
          Bucket: bucket,
          Key: `${prefix}/slots/${name}.age`,
        }),
      );
    },
  };
}
