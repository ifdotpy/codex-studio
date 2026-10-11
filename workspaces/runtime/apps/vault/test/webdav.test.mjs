// The WebDAV view over the writer's fixture: listing, content with its hash, ranges, links,
// and refusal of writes.
import { test } from "node:test";
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdtempSync, readFileSync, readdirSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, relative } from "node:path";
import { VaultReader } from "../src/reader.mjs";
import { view } from "../src/webdav.mjs";

const fixture = new URL("./fixture/", import.meta.url).pathname;
const expected = JSON.parse(
  readFileSync(join(fixture, "expected.json"), "utf8"),
);
const objects = new Map();
const walk = (dir) =>
  readdirSync(dir).forEach((n) =>
    statSync(join(dir, n)).isDirectory()
      ? walk(join(dir, n))
      : objects.set(
          relative(join(fixture, "objects"), join(dir, n)),
          new Uint8Array(readFileSync(join(dir, n))),
        ),
  );
walk(join(fixture, "objects"));

test("Finder's requests get the latest state, read only", async () => {
  const reader = new VaultReader({
    prefix: "proj",
    keys: [...objects.keys()],
    get: async (k) => objects.get(k),
    identity: readFileSync(join(fixture, "identity.txt"), "utf8").trim(),
  });
  const server = view({
    load: async () => ({ files: await reader.files(), time: 1791700000 }),
    read: (e) => reader.read(e),
    cache: mkdtempSync(join(tmpdir(), "vault-view-")),
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const base = `http://127.0.0.1:${server.address().port}`;
  try {
    const root = await fetch(base + "/", {
      method: "PROPFIND",
      headers: { depth: "1" },
    });
    assert.equal(root.status, 207);
    const listing = await root.text();
    for (const name of ["/src/", "/notes/", "/link", "/bin/"])
      assert.ok(listing.includes(`<D:href>${name}</D:href>`), name);
    assert.ok(!listing.includes("empty.txt"), "deleted in state 3");
    const app = Buffer.from(
      await (await fetch(base + "/src/app.py")).arrayBuffer(),
    );
    assert.equal(
      createHash("sha256").update(app).digest("hex"),
      expected.states[3]["src/app.py"][0],
    );
    const part = await fetch(base + "/src/app.py", {
      headers: { range: "bytes=10-19" },
    });
    assert.equal(part.status, 206);
    assert.deepEqual(
      Buffer.from(await part.arrayBuffer()),
      app.subarray(10, 20),
    );
    assert.deepEqual(
      Buffer.from(await (await fetch(base + "/link")).arrayBuffer()),
      app,
    );
    assert.equal(
      (await fetch(base + "/src/app.py", { method: "PUT", body: "x" })).status,
      405,
    );
    assert.equal(
      (
        await fetch(base + "/._app.py", {
          method: "PROPFIND",
          headers: { depth: "0" },
        })
      ).status,
      404,
    );
    assert.equal(
      (await fetch(base + "/", { method: "OPTIONS" })).headers.get("dav"),
      "1",
    );
  } finally {
    server.close();
  }
});
