import assert from "node:assert/strict";
import { EventEmitter } from "node:events";
import { PassThrough } from "node:stream";
import test from "node:test";
import { waitForFixturePort } from "./fixture-readiness.ts";

class FakeChild extends EventEmitter {
  stdout = new PassThrough();
}

test("fixture readiness parses a port across output chunks", async () => {
  const child = new FakeChild();
  const ready = waitForFixturePort(child, { timeoutMs: 100 });
  child.stdout.write("41");
  child.stdout.write("23\n");
  assert.equal(await ready, 4123);
});

test("fixture readiness reports early exit instead of waiting for timeout", async () => {
  const child = new FakeChild();
  const ready = waitForFixturePort(child, { timeoutMs: 1000, label: "Search" });
  child.stdout.end();
  child.emit("close", 1, null);
  await assert.rejects(ready, /Search fixture exited before printing its port/);
});

test("fixture readiness has one shared, bounded startup timeout", async (t) => {
  t.mock.timers.enable({ apis: ["setTimeout"] });
  const child = new FakeChild();
  const ready = waitForFixturePort(child, { timeoutMs: 5, label: "Message" });
  const rejected = assert.rejects(
    ready,
    /Message fixture did not print its port within 5ms/,
  );
  t.mock.timers.tick(5);
  await rejected;
  child.stdout.destroy();
});
