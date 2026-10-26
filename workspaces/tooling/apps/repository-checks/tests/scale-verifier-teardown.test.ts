import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { EventEmitter } from "node:events";
import test from "node:test";
import type { TestContext } from "node:test";
import type { FixtureMessage } from "./fixtures/scale-verifier-child.ts";
import { mapLimited } from "../../../../client/apps/web/scripts/uiTeamScaleAudit.mjs";
import { fileURLToPath } from "node:url";

// A separate process receives real POSIX signals. Every gate is acknowledged
// over IPC; no timing sleeps, live server, provider work or synthetic load.
const fixture = new URL("./fixtures/scale-verifier-child.ts", import.meta.url);

function start(t: TestContext, mode: string) {
  const child = spawn(process.execPath, [fileURLToPath(fixture), mode], {
    stdio: ["ignore", "ignore", "pipe", "ipc"],
  });
  t.after(() => {
    if (child.exitCode === null) child.kill("SIGKILL");
  });
  const events = new EventEmitter<{ change: [] }>();
  const messages: FixtureMessage[] = [];
  let stderr = "";
  let exit: { code: number | null; signal: NodeJS.Signals | null } | null =
    null;
  child.on("message", (message: FixtureMessage) => {
    messages.push(message);
    events.emit("change");
  });
  assert.ok(child.stderr);
  child.stderr.on("data", (data: Buffer) => {
    stderr += data;
    events.emit("change");
  });
  child.on("close", (code, signal) => {
    exit = { code, signal };
    events.emit("change");
  });
  const until = async (predicate: () => boolean) => {
    while (!predicate()) {
      assert.equal(
        exit,
        null,
        `Child exited prematurely: ${JSON.stringify(exit)} ${stderr}`,
      );
      await new Promise<void>((resolve) => events.once("change", resolve));
    }
  };
  return {
    child,
    messages,
    get stderr() {
      return stderr;
    },
    get exit() {
      return exit;
    },
    stage: (stage: string) =>
      until(() => messages.some((message) => message.stage === stage)),
    signal: async (signal: NodeJS.Signals, count: number) => {
      assert.ok(child.kill(signal));
      await until(
        () => stderr.split("waiting for teardown.").length - 1 >= count,
      );
    },
    release: (stage: string) => child.send({ stage }),
    finished: () => until(() => exit !== null),
  };
}

function assertOneDelete(run: ReturnType<typeof start>) {
  assert.deepEqual(
    run.messages
      .filter((message) => message.method)
      .map(({ method, path }) => `${method} ${path}`),
    ["GET /api/transcript/page?id=lead-1", "POST /api/conversation/delete"],
  );
}

for (const mode of [
  "normal",
  "error",
  "assertion",
  "save-error",
  "audit-error",
  "lost-delete-reply",
]) {
  test(
    `teardown once after ${mode}, with audit before delete`,
    { timeout: 5_000 },
    async (t) => {
      const run = start(t, mode);
      await run.stage("work");
      run.release("work");
      await run.stage("audit");
      run.release("audit");
      await run.stage("delete");
      run.release("delete");
      await run.finished();
      assertOneDelete(run);
      assert.ok(run.exit);
      assert.equal(run.exit.signal, null);
      assert.ok(run.exit);
      assert.equal(run.exit.code, mode === "normal" ? 0 : 1);
    },
  );
}

for (const signal of ["SIGINT", "SIGTERM"] satisfies NodeJS.Signals[]) {
  test(
    `${signal} and a repeated signal during cleanup wait for one delete`,
    { timeout: 5_000 },
    async (t) => {
      const run = start(t, "normal");
      await run.stage("work");
      await run.signal(signal, 1);
      await run.stage("audit");
      run.release("audit");
      await run.stage("delete");
      await run.signal(signal, 2);
      assert.equal({ ...run }.exit, null);
      run.release("delete");
      await run.finished();
      assertOneDelete(run);
      assert.deepEqual(
        run.messages.find((message) => message.stage === "cleaned")?.remaining,
        [],
      );
      assert.ok(run.exit);
      assert.equal(run.exit.signal, null);
      assert.ok(run.exit);
      assert.equal(run.exit.code, signal === "SIGINT" ? 130 : 143);
    },
  );
}

test(
  "signals first received during teardown do not interrupt it",
  { timeout: 5_000 },
  async (t) => {
    const run = start(t, "normal");
    await run.stage("work");
    run.release("work");
    await run.stage("audit");
    await run.signal("SIGINT", 1);
    run.release("audit");
    await run.stage("delete");
    await run.signal("SIGTERM", 2);
    run.release("delete");
    await run.finished();
    assertOneDelete(run);
    assert.ok(run.exit);
    assert.equal(run.exit.code, 143);
  },
);

test(
  "third signal reports remaining ownership before force exit",
  { timeout: 5_000 },
  async (t) => {
    const run = start(t, "normal");
    await run.stage("work");
    await run.signal("SIGINT", 1);
    await run.stage("audit");
    run.release("audit");
    await run.stage("delete");
    await run.signal("SIGTERM", 2);
    assert.ok(run.child.kill("SIGINT"));
    await run.finished();
    assertOneDelete(run);
    assert.ok(run.exit);
    assert.equal(run.exit.code, 130);
    assert.match(
      run.stderr,
      /Forced exit.*remaining ownership.*lead-1.*worker-1.*"deleteAttempts":1/,
    );
  },
);

test("failed parallel audit waits for every in-flight lane before teardown", async () => {
  const { promise: pending, resolve: release } = Promise.withResolvers<void>();
  const stages: string[] = [];
  const audit: Promise<void[]> = mapLimited(
    ["failed", "pending"],
    2,
    async (id: string) => {
      if (id === "failed") throw new Error("transcript failed");
      await pending;
      stages.push("audit settled");
    },
  );
  const finished = audit.catch(() => {
    stages.push("cleanup");
  });
  await new Promise(setImmediate);
  assert.deepEqual(stages, []);
  release();
  await finished;
  assert.deepEqual(stages, ["audit settled", "cleanup"]);
});
