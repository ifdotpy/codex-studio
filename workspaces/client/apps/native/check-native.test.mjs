import assert from "node:assert/strict";
import { test } from "node:test";
import { createFixture } from "./check-native.mjs";

test("native fixture preserves identity refusal, byte replies, and draft tombstones", async () => {
  const fixture = await createFixture("/fixture/state");
  try {
    const root = `http://127.0.0.1:${fixture.port}`;
    assert.equal(
      (await (await fetch(`${root}/api/desktop`)).json()).stateDir,
      "/fixture/state/wrong-state",
    );
    fixture.acceptIdentity();
    assert.equal(
      (await (await fetch(`${root}/api/desktop`)).json()).stateDir,
      "/fixture/state",
    );
    const result = await fetch(`${root}/api/native-check`, {
      method: "POST",
      body: "null",
      headers: { "X-Studio-Request-Id": "original-http-command" },
    });
    assert.equal(result.status, 409);
    assert.equal(result.headers.get("x-studio-api-schema-mismatch"), "1");
    assert.deepEqual(
      Buffer.from(await result.arrayBuffer()),
      Buffer.from([97, 0, 255]),
    );
    assert.equal(
      fixture.commands[0].headers["x-studio-request-id"],
      "original-http-command",
    );
    const draft = (document) =>
      fetch(`${root}/api/sync/drafts`, {
        method: "POST",
        body: JSON.stringify({ rows: [{ newDocumentState: document }] }),
      });
    assert.equal((await draft({ deleted: false })).status, 200);
    assert.equal((await draft({ _deleted: false })).status, 400);
    assert.equal((await draft({ deleted: "false" })).status, 400);
    const stream = await (
      await fetch(`${root}/api/native-check-events`)
    ).text();
    assert.match(stream, /data: Привет 🦀\ndata: second/);
  } finally {
    await fixture.close();
  }
});
