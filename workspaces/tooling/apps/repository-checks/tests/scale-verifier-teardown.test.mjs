import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { EventEmitter } from "node:events";
import test from "node:test";

const teardownUrl = new URL(
  "../../../../client/apps/web/scripts/scaleVerifierTeardown.ts",
  import.meta.url,
).href;
const auditUrl = new URL(
  "../../../../client/apps/web/scripts/uiTeamScaleAudit.mjs",
  import.meta.url,
).href;

// A separate process receives real POSIX signals. Every gate is acknowledged
// over IPC; no timing sleeps, live server, provider work or synthetic load.
const fixture = `
import assert from "node:assert/strict";
import { runScaleVerifier } from ${JSON.stringify(teardownUrl)};
const mode = process.argv[1];
const gates = new Map();
const wait = (stage) => new Promise((resolve) => {
  gates.set(stage, resolve);
  process.send({ stage });
});
process.on("message", ({ stage }) => gates.get(stage)?.());
// The lifecycle must keep unresolved work and teardown alive without I/O.
process.channel.unref();
let interrupted = false;
let remaining = ["lead-1", "worker-1"];
let deletes = 0;
const api = async (method, path, body) => {
  process.send({ method, path, body });
  if (method === "GET") {
    await wait("audit");
    if (mode === "audit-error") throw new Error("audit failed");
  } else {
    assert.deepEqual(body, { id: "lead-1" });
    deletes += 1;
    await wait("delete");
    if (mode === "lost-delete-reply") throw new Error("delete outcome unknown");
    remaining = [];
  }
};
await runScaleVerifier(async ({ setCleanup, setRemaining }) => {
  setRemaining(() => ({ agents: remaining, deleteAttempts: deletes }));
  setCleanup(async () => {
    try {
      await api("GET", "/api/transcript/page?id=lead-1");
    } finally {
      await api("POST", "/api/conversation/delete", { id: "lead-1" });
    }
    process.send({ stage: "cleaned", remaining, deletes });
  });
  await wait("work");
  if (mode === "error") throw new Error("work failed");
  if (mode === "assertion") assert.fail("work assertion");
  if (mode === "save-error") throw new Error("evidence write failed");
  if (interrupted) throw new Error("interrupted");
}, {
  onInterrupt() {
    interrupted = true;
    gates.get("work")?.();
  },
}).catch((error) => {
  process.stderr.write(error.message + "\\n");
  process.exitCode ||= 1;
});
process.disconnect();
`;

function start(t, mode) {
  const child = spawn(
    process.execPath,
    ["--input-type=module", "-e", fixture, mode],
    {
      stdio: ["ignore", "ignore", "pipe", "ipc"],
    },
  );
  t.after(() => {
    if (child.exitCode === null) child.kill("SIGKILL");
  });
  const events = new EventEmitter();
  const messages = [];
  let stderr = "";
  let exit = null;
  child.on("message", (message) => {
    messages.push(message);
    events.emit("change");
  });
  child.stderr.on("data", (data) => {
    stderr += data;
    events.emit("change");
  });
  child.on("close", (code, signal) => {
    exit = { code, signal };
    events.emit("change");
  });
  const until = async (predicate) => {
    while (!predicate()) {
      assert.equal(
        exit,
        null,
        `Child exited prematurely: ${JSON.stringify(exit)} ${stderr}`,
      );
      await new Promise((resolve) => events.once("change", resolve));
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
    stage: (stage) =>
      until(() => messages.some((message) => message.stage === stage)),
    signal: async (signal, count) => {
      assert.ok(child.kill(signal));
      await until(
        () => stderr.split("waiting for teardown.").length - 1 >= count,
      );
    },
    release: (stage) => child.send({ stage }),
    finished: () => until(() => exit !== null),
  };
}

function assertOneDelete(run) {
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
      assert.equal(run.exit.signal, null);
      assert.equal(run.exit.code, mode === "normal" ? 0 : 1);
    },
  );
}

for (const signal of ["SIGINT", "SIGTERM"]) {
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
      assert.equal(run.exit, null);
      run.release("delete");
      await run.finished();
      assertOneDelete(run);
      assert.deepEqual(
        run.messages.find((message) => message.stage === "cleaned").remaining,
        [],
      );
      assert.equal(run.exit.signal, null);
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
    assert.equal(run.exit.code, 130);
    assert.match(
      run.stderr,
      /Forced exit.*remaining ownership.*lead-1.*worker-1.*"deleteAttempts":1/,
    );
  },
);

test("failed parallel audit waits for every in-flight lane before teardown", async () => {
  const { mapLimited } = await import(auditUrl);
  let release;
  const pending = new Promise((resolve) => {
    release = resolve;
  });
  const stages = [];
  const audit = mapLimited(["failed", "pending"], 2, async (id) => {
    if (id === "failed") throw new Error("transcript failed");
    await pending;
    stages.push("audit settled");
  });
  const finished = audit.catch(() => {
    stages.push("cleanup");
  });
  await new Promise(setImmediate);
  assert.deepEqual(stages, []);
  release();
  await finished;
  assert.deepEqual(stages, ["audit settled", "cleanup"]);
});
