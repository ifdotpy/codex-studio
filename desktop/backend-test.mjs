import assert from "node:assert/strict";
import { createServer } from "node:http";
import { test } from "node:test";
import { realpathSync } from "node:fs";
import { tmpdir } from "node:os";
import { createRequire } from "node:module";
const { identity, ensureBackend } = createRequire(import.meta.url)("./backend.cjs");
const state = "/unused-studio-state";
const record = { application: "codex-agents", protocol: 1, pid: process.pid, stateDir: state };
const timing = { attemptTimeoutMs: 50, timeoutMs: 250, retryDelayMs: 10 };

async function serve(handler, run) {
  const server = createServer(handler);
  await new Promise(resolve => server.listen(0, "127.0.0.1", resolve));
  try {
    await run(`http://127.0.0.1:${server.address().port}`);
  } finally {
    server.closeAllConnections();
    await new Promise(resolve => server.close(resolve));
  }
}

test("a valid backend can take longer than the old one-second limit", async () => {
  await serve((req, res) => setTimeout(() => res.end(JSON.stringify(record)), 1200), async origin => {
    assert.deepEqual(await identity(origin, state), record);
  });
});

test("retry a timed-out identity read", async () => {
  let calls = 0;
  await serve((req, res) => {
    if (++calls === 1) return;
    res.end(JSON.stringify(record));
  }, async origin => {
    assert.deepEqual(await identity(origin, state, timing), record);
    assert.equal(calls, 2);

  });
});

test("retry a timeout while reading the response body", async () => {
  let calls = 0;
  await serve((req, res) => {
    res.writeHead(200, { "content-type": "application/json" });
    if (++calls === 1) return res.write("{");
    res.end(JSON.stringify(record));
  }, async origin => {
    assert.deepEqual(await identity(origin, state, timing), record);
    assert.equal(calls, 2);
  });
});

test("a server that never responds reaches a bounded failure", async () => {
  await serve(() => {}, async origin => {
    await assert.rejects(identity(origin, state, timing), /No replacement backend was started/);
  });
});

test("a refusal after a timeout cannot authorize a replacement", async () => {
  const original = globalThis.fetch;
  let calls = 0;
  globalThis.fetch = async (url, { signal }) => {
    if (++calls > 1) throw new TypeError("fetch failed", { cause: { code: "ECONNREFUSED" } });
    return new Promise((resolve, reject) => signal.addEventListener("abort", () => reject(signal.reason)));
  };
  const keepAlive = setInterval(() => {}, 1000);
  try {
    await assert.rejects(identity("http://127.0.0.1:4620", state, timing), /No replacement backend was started/);
  } finally {
    clearInterval(keepAlive);
    globalThis.fetch = original;
  }
});

test("incompatible identity and invalid JSON fail without retries", async () => {
  for (const body of ["not JSON", JSON.stringify({ ...record, stateDir: "/different" })]) {
    let calls = 0;
    await serve((req, res) => { calls++; res.end(body); }, async origin => {
      await assert.rejects(identity(origin, state, timing), /incompatible/);
      assert.equal(calls, 1);
    });
  }
});


test("a slow existing backend attaches without spawning a process", async () => {
  const canonicalState = realpathSync(tmpdir());
  const data = { ...record, stateDir: canonicalState };
  await serve((req, res) => setTimeout(() => res.end(JSON.stringify(data)), 1200), async origin => {
    const result = await ensureBackend({
      resources: "/missing-desktop-assets",
      port: Number(new URL(origin).port),
      env: { CODEX_AGENTS_STATE_DIR: canonicalState },
    });
    assert.deepEqual(result, { ...data, origin, owned: false });
  });
});
