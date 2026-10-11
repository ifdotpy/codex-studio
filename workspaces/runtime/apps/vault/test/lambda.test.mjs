// The vault function with a fake S3: the challenge proves the vault identity, the token is
// short and bound to the vault, and only state objects and passkey slots are reachable.
import { test } from "node:test";
import assert from "node:assert/strict";
import * as age from "age-encryption";
import { makeHandler } from "../lambda/handler.mjs";

function fakeS3() {
  const objects = new Map([
    ["p/states/0000000001.aaaaaaaa.tree.age", "t"],
    ["p/slots/paper.age", "paper-slot"],
    ["other/states/0000000001.bbbbbbbb.tree.age", "x"],
  ]);
  return {
    objects,
    async send(command) {
      const input = command.input;
      switch (command.constructor.name) {
        case "ListObjectsV2Command":
          return {
            Contents: [...objects.keys()]
              .filter((k) => k.startsWith(input.Prefix))
              .map((Key) => ({ Key })),
          };
        case "GetObjectCommand":
          return {
            Body: {
              transformToByteArray: async () =>
                new TextEncoder().encode(objects.get(input.Key)),
            },
          };
        case "PutObjectCommand":
          if (input.IfNoneMatch === "*" && objects.has(input.Key))
            throw new Error("PreconditionFailed");
          objects.set(input.Key, Buffer.from(input.Body).toString());
          return {};
        case "DeleteObjectCommand":
          objects.delete(input.Key);
          return {};
      }
      throw new Error(command.constructor.name);
    },
  };
}

async function setup(clock = { t: 1_000_000 }) {
  const identity = await age.generateIdentity();
  const s3 = fakeS3();
  const env = {
    BUCKET: "b",
    PREFIX: "p",
    RECIPIENT: await age.identityToRecipient(identity),
    TOKEN_SECRET: "s".repeat(64),
  };
  const handler = makeHandler({
    s3,
    env,
    now: () => clock.t,
    presign: async (_c, command) => `https://signed/${command.input.Key}`,
  });
  const call = async (method, rawPath, body) => {
    const response = await handler({
      requestContext: { http: { method } },
      rawPath,
      body: body && JSON.stringify(body),
    });
    return { status: response.statusCode, body: JSON.parse(response.body) };
  };
  const token = async () => {
    const { body } = await call("GET", "/challenge");
    const decrypter = new age.Decrypter();
    decrypter.addIdentity(identity);
    return new TextDecoder().decode(
      await decrypter.decrypt(Buffer.from(body.challenge, "base64")),
    );
  };
  return { call, token, s3, clock };
}

test("the holder of the vault identity lists and presigns state objects", async () => {
  const { call, token } = await setup();
  const t = await token();
  const listed = await call("POST", "/presign", {
    token: t,
    list: true,
    keys: ["p/states/0000000001.aaaaaaaa.tree.age"],
  });
  assert.equal(listed.status, 200);
  assert.deepEqual(listed.body.keys, ["p/states/0000000001.aaaaaaaa.tree.age"]);
  assert.equal(
    listed.body.urls["p/states/0000000001.aaaaaaaa.tree.age"],
    "https://signed/p/states/0000000001.aaaaaaaa.tree.age",
  );
  for (const keys of [
    ["other/states/0000000001.bbbbbbbb.tree.age"],
    ["p/slots/paper.age"],
    ["p/states/../slots/paper.age"],
  ]) {
    assert.equal(
      (await call("POST", "/presign", { token: t, keys })).status,
      400,
    );
  }
});

test("a wrong, forged or expired token is refused", async () => {
  const { call, token, clock } = await setup();
  const t = await token();
  const [expiry] = t.split(".");
  for (const bad of [undefined, "", "123.abc", `${expiry}.${"0".repeat(64)}`]) {
    assert.equal(
      (await call("POST", "/presign", { token: bad, list: true })).status,
      401,
    );
  }
  clock.t += 11 * 60 * 1000;
  assert.equal(
    (await call("POST", "/presign", { token: t, list: true })).status,
    401,
  );
});

test("slots are public to read, and only passkey slots change, once", async () => {
  const { call, token, s3 } = await setup();
  const slots = await call("GET", "/slots");
  assert.deepEqual(
    slots.body.slots.map((s) => s.name),
    ["paper"],
  );
  const t = await token();
  const data = Buffer.from("slot").toString("base64");
  assert.equal(
    (await call("PUT", "/slot", { token: t, name: "passkey-abc_1", data }))
      .status,
    200,
  );
  await assert.rejects(
    call("PUT", "/slot", { token: t, name: "passkey-abc_1", data }),
  );
  for (const name of ["paper", "passkey-../x", "passkey-"]) {
    assert.equal(
      (await call("PUT", "/slot", { token: t, name, data })).status,
      400,
    );
    assert.equal(
      (await call("DELETE", "/slot", { token: t, name })).status,
      400,
    );
  }
  assert.equal(
    (await call("DELETE", "/slot", { token: t, name: "passkey-abc_1" })).status,
    200,
  );
  assert.equal(s3.objects.has("p/slots/paper.age"), true);
  assert.equal(s3.objects.has("p/slots/passkey-abc_1.age"), false);
});
