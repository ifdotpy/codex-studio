import type { BridgeHarness, BridgeNoteFor } from "./testing/bridge-harness.ts";
import { waitForFile } from "./testing/file-signals.ts";
// Preserve the Stop receipt contracts after removal of the Python bridge tests.
import { afterEach, test } from "vitest";
import assert from "node:assert/strict";
import fs from "node:fs/promises";
import { existsSync } from "node:fs";
import path from "node:path";
import { startBridge } from "./testing/bridge-harness.ts";

const SDK = new URL("./stop-receipts.fixture.mts", import.meta.url);
const bridges: BridgeHarness[] = [];
afterEach(async () => {
  for (const bridge of bridges.splice(0)) {
    // Release injected barriers even if an assertion fails.
    await fs.writeFile(path.join(bridge.root, ".release-save"), "");
    await fs.writeFile(path.join(bridge.root, ".release-account"), "");
    await bridge.close();
  }
});
async function fixture() {
  const bridge = await startBridge(SDK, "claude-stop-receipts-");
  bridges.push(bridge);
  return bridge;
}
const start = (bridge: BridgeHarness, id: string) =>
  bridge.call("turn/start", {
    threadId: bridge.thread,
    clientUserMessageId: id,
    input: [{ type: "text", text: id === "blocked-save" ? "/clear" : id }],
  });
const completed = async (bridge: BridgeHarness, id: string) =>
  (
    await bridge.note(
      (n): n is BridgeNoteFor<"turn/completed"> =>
        n.method === "turn/completed" && n.params.turn.id === id,
    )
  ).params.turn;
const marker = (bridge: BridgeHarness, name: string) =>
  waitForFile(path.join(bridge.root, name));
const admissions = async (bridge: BridgeHarness) =>
  existsSync(path.join(bridge.root, ".admitted"))
    ? (await fs.readFile(path.join(bridge.root, ".admitted"), "utf8"))
        .trim()
        .split("\n")
    : [];

test("Stop replies before a reserved input save completes and the rejection survives restart", async () => {
  const bridge = await fixture();
  const warm = await start(bridge, "warm");
  assert.equal((await completed(bridge, warm.turn.id)).status, "completed");
  await fs.writeFile(path.join(bridge.root, ".hold-save"), "");
  const pending = start(bridge, "blocked-save").then(
    (value) => ({ value }),
    (error: unknown) => ({ error }),
  );
  await marker(bridge, ".save-held");
  const receipt = await bridge.call("turn/interrupt", {
    threadId: bridge.thread,
  });
  assert.deepEqual(receipt, { cancelled: [], still_queued: [] });
  assert.equal(existsSync(path.join(bridge.root, ".release-save")), false);
  assert.equal((await admissions(bridge)).length, 1);
  await fs.writeFile(path.join(bridge.root, ".release-save"), "");
  const result = await pending;
  assert.ok("error" in result && result.error instanceof Error);
  assert.match(result.error.message, /stopped before accepting/);
  const history = await bridge.call("thread/turns/list", {
    threadId: bridge.thread,
  });
  assert.equal(
    history.data.find((turn) => turn.clientUserMessageId === "blocked-save")
      ?.startOutcome,
    "not_applied",
  );
  await bridge.restart();
  const retry = await start(bridge, "blocked-save");
  assert.equal((await completed(bridge, retry.turn.id)).status, "completed");
  assert.equal((await start(bridge, "blocked-save")).turn.id, retry.turn.id);
  assert.equal((await admissions(bridge)).length, 2);
});

test("Stop preserves a rejected pre-admission receipt and permits one exact retry after restart", async () => {
  const bridge = await fixture();
  await fs.writeFile(path.join(bridge.root, ".hold-account"), "");
  const original = await start(bridge, "before-admission");
  await marker(bridge, ".account-entered");
  const receipt = await bridge.call("turn/interrupt", {
    threadId: bridge.thread,
    turnId: original.turn.id,
  });
  assert.deepEqual(receipt, {
    cancelled: ["before-admission"],
    still_queued: [],
  });
  await fs.writeFile(path.join(bridge.root, ".release-account"), "");
  const stopped = await completed(bridge, original.turn.id);
  assert.equal(stopped.status, "interrupted");
  assert.equal(stopped.startOutcome, "not_applied");
  assert.equal(stopped.error?.data?.turnStartOutcome, "not_applied");
  assert.deepEqual(await admissions(bridge), []);
  await fs.rm(path.join(bridge.root, ".hold-account"));
  await bridge.restart();
  const saved = await bridge.call("thread/turns/list", {
    threadId: bridge.thread,
  });
  assert.equal(saved.data[0]?.startOutcome, "not_applied");
  const retry = await start(bridge, "before-admission");
  assert.notEqual(retry.turn.id, original.turn.id);
  assert.equal((await completed(bridge, retry.turn.id)).status, "completed");
  assert.equal(
    (await start(bridge, "before-admission")).turn.id,
    retry.turn.id,
  );
  assert.equal((await admissions(bridge)).length, 1);
});

test("Stop before initial clear admission permits a live exact retry", async () => {
  const bridge = await fixture();
  const warm = await start(bridge, "warm");
  assert.equal((await completed(bridge, warm.turn.id)).status, "completed");
  await fs.writeFile(path.join(bridge.root, ".hold-save"), "");
  const pending = start(bridge, "blocked-save").then(
    (value) => ({ value }),
    (error: unknown) => ({ error }),
  );
  await marker(bridge, ".save-held");
  const receipt = await bridge.call("turn/interrupt", {
    threadId: bridge.thread,
  });
  assert.deepEqual(receipt, { cancelled: [], still_queued: [] });
  assert.equal(existsSync(path.join(bridge.root, ".release-save")), false);
  assert.equal((await admissions(bridge)).length, 1);
  await fs.writeFile(path.join(bridge.root, ".release-save"), "");
  const result = await pending;
  assert.ok("error" in result && result.error instanceof Error);
  assert.match(result.error.message, /stopped before accepting/);
  const history = await bridge.call("thread/turns/list", {
    threadId: bridge.thread,
  });
  assert.equal(
    history.data.find((turn) => turn.clientUserMessageId === "blocked-save")
      ?.startOutcome,
    "not_applied",
  );
  const retry = await start(bridge, "blocked-save");
  assert.equal((await completed(bridge, retry.turn.id)).status, "completed");
  assert.equal((await start(bridge, "blocked-save")).turn.id, retry.turn.id);
  assert.equal((await admissions(bridge)).length, 2);
});
