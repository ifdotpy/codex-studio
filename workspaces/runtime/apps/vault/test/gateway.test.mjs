// The gateway with a fake S3: the challenge proves the vault identity, the token is short and
// bound to one vault, only state objects and passkey slots are reachable, and the host is checked.
import { test } from "node:test";
import assert from "node:assert/strict";
import * as age from "age-encryption";
import { mkdtempSync, mkdirSync, writeFileSync } from "node:fs";
import http from "node:http";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { Readable } from "node:stream";
import { makeVault } from "../src/service.mjs";
import { makeGateway } from "../gateway/server.mjs";

function fakeS3(objects) {
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
        case "GetObjectCommand": {
          if (!objects.has(input.Key))
            throw Object.assign(new Error("missing"), { name: "NoSuchKey" });
          const data = Buffer.from(objects.get(input.Key));
          return {
            ContentLength: data.length,
            Body: Object.assign(Readable.from([data]), {
              transformToByteArray: async () => data,
            }),
          };
        }
        case "PutObjectCommand":
          if (input.IfNoneMatch === "*" && objects.has(input.Key))
            throw Object.assign(new Error("exists"), {
              name: "PreconditionFailed",
            });
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

const TREE = "0000000001.aaaaaaaa.tree.age";

async function setup(t) {
  const clock = { t: 1_000_000 };
  const identity = await age.generateIdentity();
  const s3 = fakeS3(
    new Map([
      [`p/states/${TREE}`, "tree"],
      ["p/slots/paper.age", "paper-slot"],
      ["q/states/0000000001.bbbbbbbb.tree.age", "other"],
    ]),
  );
  const secret = Buffer.alloc(32, 7);
  const recipient = await age.identityToRecipient(identity);
  const vaults = Object.fromEntries(
    ["p", "q"].map((prefix) => [
      prefix,
      makeVault({
        s3,
        bucket: "b",
        prefix,
        recipient,
        secret,
        now: () => clock.t,
      }),
    ]),
  );
  const site = mkdtempSync(join(tmpdir(), "vault-site-"));
  mkdirSync(join(site, "assets"));
  writeFileSync(join(site, "index.html"), "<!doctype html>index");
  writeFileSync(join(site, "assets", "app-1.js"), "js");
  const handle = makeGateway({
    config: () => ({ host: "desk.example", projects: { p: {}, q: {} } }),
    vaultFor: (name) => vaults[name] ?? null,
    site,
  });
  const server = http.createServer(handle);
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  t.after(() => server.close());
  // fetch() cannot set the Host header; http.request can.
  const call = (method, path, { token, body, host = "desk.example" } = {}) =>
    new Promise((resolve, reject) => {
      const payload = body && JSON.stringify(body);
      const request = http.request(
        {
          host: "127.0.0.1",
          port: server.address().port,
          method,
          path,
          headers: {
            host,
            ...(token ? { authorization: `Bearer ${token}` } : {}),
            ...(payload ? { "content-type": "application/json" } : {}),
          },
        },
        (response) => {
          const chunks = [];
          response.on("data", (c) => chunks.push(c));
          response.on("end", () => {
            const text = Buffer.concat(chunks).toString();
            let json;
            try {
              json = JSON.parse(text);
            } catch {
              json = undefined;
            }
            resolve({
              status: response.statusCode,
              headers: response.headers,
              text,
              json,
            });
          });
        },
      );
      request.on("error", reject);
      request.end(payload);
    });
  const token = async (project = "p") => {
    const { json } = await call("GET", `/api/p/${project}/challenge`);
    const decrypter = new age.Decrypter();
    decrypter.addIdentity(identity);
    return new TextDecoder().decode(
      await decrypter.decrypt(Buffer.from(json.challenge, "base64")),
    );
  };
  return { call, token, s3, clock };
}

test("the holder of the vault identity lists and reads state objects", async (t) => {
  const { call, token } = await setup(t);
  const tk = await token();
  const listed = await call("GET", "/api/p/p/states", { token: tk });
  assert.equal(listed.status, 200);
  assert.deepEqual(listed.json.states, [TREE]);
  const object = await call("GET", `/api/p/p/states/${TREE}`, { token: tk });
  assert.equal(object.status, 200);
  assert.equal(object.text, "tree");
  assert.match(object.headers["content-security-policy"], /default-src 'self'/);
  for (const name of [
    "..%2Fslots%2Fpaper.age",
    "paper.age",
    "0000000001.aaaaaaaa.tree.age.bak",
  ])
    assert.equal(
      (await call("GET", `/api/p/p/states/${name}`, { token: tk })).status,
      400,
    );
  assert.equal(
    (
      await call("GET", "/api/p/p/states/0000000002.aaaaaaaa.tree.age", {
        token: tk,
      })
    ).status,
    404,
  );
});

test("a token opens only its own vault, and expires", async (t) => {
  const { call, token, clock } = await setup(t);
  const tk = await token("p");
  const [expiry] = tk.split(".");
  for (const bad of [undefined, "", "123.abc", `${expiry}.${"0".repeat(64)}`])
    assert.equal(
      (await call("GET", "/api/p/p/states", { token: bad })).status,
      401,
    );
  assert.equal(
    (await call("GET", "/api/p/q/states", { token: tk })).status,
    401,
  );
  clock.t += 11 * 60 * 1000;
  assert.equal(
    (await call("GET", "/api/p/p/states", { token: tk })).status,
    401,
  );
});

test("slots are open to read, and only passkey slots change, once", async (t) => {
  const { call, token, s3 } = await setup(t);
  const slots = await call("GET", "/api/p/p/slots");
  assert.deepEqual(
    slots.json.slots.map((s) => s.name),
    ["paper"],
  );
  const tk = await token();
  const data = Buffer.from("slot").toString("base64");
  const add = () =>
    call("PUT", "/api/p/p/slots/passkey-abc_1", { token: tk, body: { data } });
  assert.equal((await add()).status, 200);
  assert.equal((await add()).status, 409);
  for (const name of ["paper", "passkey-", "passkey-a.b"]) {
    assert.equal(
      (
        await call("PUT", `/api/p/p/slots/${name}`, {
          token: tk,
          body: { data },
        })
      ).status,
      400,
    );
    assert.equal(
      (await call("DELETE", `/api/p/p/slots/${name}`, { token: tk })).status,
      400,
    );
  }
  assert.equal(
    (await call("PUT", "/api/p/p/slots/passkey-x", { body: { data } })).status,
    401,
  );
  assert.equal(
    (await call("DELETE", "/api/p/p/slots/passkey-abc_1", { token: tk }))
      .status,
    200,
  );
  assert.equal(s3.objects.has("p/slots/paper.age"), true);
  assert.equal(s3.objects.has("p/slots/passkey-abc_1.age"), false);
});

test("the site, the project list and the host check", async (t) => {
  const { call } = await setup(t);
  assert.deepEqual((await call("GET", "/api/projects")).json.projects, [
    "p",
    "q",
  ]);
  for (const path of ["/", "/p/", "/p/anything"])
    assert.equal((await call("GET", path)).text, "<!doctype html>index");
  const asset = await call("GET", "/assets/app-1.js");
  assert.equal(asset.text, "js");
  assert.match(asset.headers["cache-control"], /immutable/);
  assert.equal((await call("GET", "/assets/missing.js")).status, 404);
  assert.equal((await call("GET", "/%2e%2e/etc/passwd.txt")).status, 404);
  assert.equal((await call("GET", "/api/p/nope/slots")).status, 404);
  assert.equal((await call("POST", "/")).status, 405);
  assert.equal((await call("GET", "/", { host: "evil.example" })).status, 421);
});
