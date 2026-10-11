// The JavaScript reader against objects that the real writer made with age and zstandard
// (test/make_fixture.py), and the slot keys.
import { test } from "node:test";
import assert from "node:assert/strict";
import { createHash, randomBytes } from "node:crypto";
import { readFileSync, readdirSync, statSync } from "node:fs";
import { join, relative } from "node:path";
import { VaultReader } from "../src/reader.mjs";
import {
  bech32Decode,
  generateIdentity,
  identityFromSecret,
  openSlot,
  recipientOf,
  sealSlot,
} from "../src/keys.mjs";

const fixture = new URL("./fixture/", import.meta.url).pathname;
const expected = JSON.parse(
  readFileSync(join(fixture, "expected.json"), "utf8"),
);
const identity = readFileSync(join(fixture, "identity.txt"), "utf8").trim();

function objects() {
  const root = join(fixture, "objects");
  const found = new Map();
  const walk = (dir) => {
    for (const name of readdirSync(dir)) {
      const path = join(dir, name);
      if (statSync(path).isDirectory()) walk(path);
      else found.set(relative(root, path), new Uint8Array(readFileSync(path)));
    }
  };
  walk(root);
  return found;
}

test("every pushed state reads back with the writer's content", async () => {
  const store = objects();
  const reader = new VaultReader({
    prefix: "proj",
    keys: [...store.keys()],
    get: async (k) => store.get(k),
    identity,
  });
  assert.deepEqual(
    reader.states().map((s) => s.seq),
    [1, 2, 3, 4],
  );
  for (const [index, state] of expected.states.entries()) {
    const files = await reader.files(index + 1);
    assert.deepEqual(Object.keys(files).sort(), Object.keys(state).sort());
    for (const [path, [sha, mode]] of Object.entries(state)) {
      const data = await reader.read(files[path]);
      assert.equal(createHash("sha256").update(data).digest("hex"), sha, path);
      assert.equal(files[path][0], mode, path);
    }
  }
  const latest = await reader.files();
  assert.equal(latest["src/app.py"][3][0][5], "delta");
});

test("a passkey secret becomes an age identity that opens its slot", async () => {
  const vault = await generateIdentity();
  const secret = randomBytes(32);
  const opener = identityFromSecret(secret);
  assert.deepEqual(bech32Decode(opener).bytes, new Uint8Array(secret));
  assert.equal(bech32Decode(await generateIdentity()).hrp, "age-secret-key-");
  const slot = await sealSlot(vault, await recipientOf(opener));
  assert.equal(await openSlot(slot, opener), vault);
  await assert.rejects(openSlot(slot, await generateIdentity()));
});

test("the fixture recipient belongs to the fixture identity", async () => {
  assert.equal(await recipientOf(identity), expected.recipient);
});
