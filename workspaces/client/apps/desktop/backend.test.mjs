import assert from "node:assert/strict";
import { createServer } from "node:http";
import { test } from "vitest";
import {
  realpathSync,
  mkdtempSync,
  mkdirSync,
  writeFileSync,
  readFileSync,
  rmSync,
} from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { tmpdir } from "node:os";
import { createRequire } from "node:module";
import { createHash } from "node:crypto";
const {
  identity,
  ensureBackend,
  backendBuild,
  updateStatus,
  backendExitStatus,
} = createRequire(import.meta.url)("./backend.cjs");
const repositoryRoot = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../../../..",
);
const state = "/unused-studio-state";
const record = {
  application: "codex-agents",
  protocol: 1,
  backendKind: "rust",
  backendBuild: "b".repeat(64),
  pid: process.pid,
  stateDir: state,
  diskSpace: { level: null, message: null },
};
const timing = { attemptTimeoutMs: 50, timeoutMs: 250, retryDelayMs: 10 };

test("desktop recognizes backend exit by signal during startup", () => {
  assert.equal(
    backendExitStatus({ exitCode: null, signalCode: "SIGTERM" }),
    "SIGTERM",
  );
  assert.equal(backendExitStatus({ exitCode: 23, signalCode: null }), 23);
  assert.equal(backendExitStatus({ exitCode: null, signalCode: null }), null);
});

async function serve(handler, run) {
  const server = createServer(handler);
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  try {
    await run(`http://127.0.0.1:${server.address().port}`);
  } finally {
    server.closeAllConnections();
    await new Promise((resolve) => server.close(resolve));
  }
}

test("a valid backend can take longer than the old one-second limit", async () => {
  await serve(
    (req, res) => setTimeout(() => res.end(JSON.stringify(record)), 1200),
    async (origin) => {
      assert.deepEqual(await identity(origin, state), record);
    },
  );
});

test("retry a timed-out identity read", async () => {
  let calls = 0;
  await serve(
    (req, res) => {
      if (++calls === 1) return;
      res.end(JSON.stringify(record));
    },
    async (origin) => {
      assert.deepEqual(await identity(origin, state, timing), record);
      assert.equal(calls, 2);
    },
  );
});

test("retry a timeout while reading the response body", async () => {
  let calls = 0;
  await serve(
    (req, res) => {
      res.writeHead(200, { "content-type": "application/json" });
      if (++calls === 1) return res.write("{");
      res.end(JSON.stringify(record));
    },
    async (origin) => {
      assert.deepEqual(await identity(origin, state, timing), record);
      assert.equal(calls, 2);
    },
  );
});

test("a server that never responds reaches a bounded failure", async () => {
  await serve(
    () => {},
    async (origin) => {
      await assert.rejects(
        identity(origin, state, timing),
        /No replacement backend was started/,
      );
    },
  );
});

test("a refusal after a timeout cannot authorize a replacement", async () => {
  const original = globalThis.fetch;
  let calls = 0;
  globalThis.fetch = async (url, { signal }) => {
    if (++calls > 1)
      throw new TypeError("fetch failed", { cause: { code: "ECONNREFUSED" } });
    return new Promise((resolve, reject) =>
      signal.addEventListener("abort", () => reject(signal.reason)),
    );
  };
  const keepAlive = setInterval(() => {}, 1000);
  try {
    await assert.rejects(
      identity("http://127.0.0.1:4620", state, timing),
      /No replacement backend was started/,
    );
  } finally {
    clearInterval(keepAlive);
    globalThis.fetch = original;
  }
});

test("incompatible identity and invalid JSON fail without retries", async () => {
  for (const body of [
    "not JSON",
    JSON.stringify({ ...record, stateDir: "/different" }),
    JSON.stringify({ ...record, backendKind: "python" }),
    JSON.stringify({ ...record, backendKind: undefined }),
    JSON.stringify({ ...record, protocol: 0 }),
    JSON.stringify({ ...record, protocol: 2 }),
    JSON.stringify({ ...record, backendBuild: undefined }),
    JSON.stringify({ ...record, backendBuild: null }),
    JSON.stringify({ ...record, backendBuild: 123 }),
    JSON.stringify({ ...record, backendBuild: "unknown" }),
    JSON.stringify({ ...record, backendBuild: "a".repeat(63) }),
  ]) {
    let calls = 0;
    await serve(
      (req, res) => {
        calls++;
        res.end(body);
      },
      async (origin) => {
        await assert.rejects(identity(origin, state, timing), /incompatible/);
        assert.equal(calls, 1);
      },
    );
  }
});

test("a slow existing backend attaches without spawning a process", async () => {
  const canonicalState = realpathSync(tmpdir());
  const data = { ...record, stateDir: canonicalState };
  await serve(
    (req, res) => setTimeout(() => res.end(JSON.stringify(data)), 1200),
    async (origin) => {
      const result = await ensureBackend({
        resources: "/missing-desktop-assets",
        port: Number(new URL(origin).port),
        env: {
          CODEX_AGENTS_STATE_DIR: canonicalState,
          CODEX_RUNTIME_SERVER:
            "/missing-desktop-assets/bin/studio-runtime-server",
        },
      });
      assert.deepEqual(result, {
        ...data,
        origin,
        owned: false,
        availableBackendBuild: null,
        updateRequired: null,
      });
    },
  );
});

test("supervisor mode refuses a backend without supervisor ownership", async () => {
  const canonicalState = realpathSync(tmpdir());
  await serve(
    (_req, res) =>
      res.end(JSON.stringify({ ...record, stateDir: canonicalState })),
    async (origin) => {
      await assert.rejects(
        ensureBackend({
          resources: "/missing-desktop-assets",
          port: Number(new URL(origin).port),
          env: {
            CODEX_AGENTS_STATE_DIR: canonicalState,
            CODEX_AGENTS_SUPERVISOR_MODE: "1",
          },
        }),
        /does not use it/,
      );
    },
  );
});

test("saved supervisor mode attaches consistently when Finder supplies no variable", async () => {
  const root = mkdtempSync(path.join(tmpdir(), "studio-saved-supervisor-"));
  const canonicalState = path.join(root, "state");
  mkdirSync(canonicalState);
  writeFileSync(
    path.join(canonicalState, "background-recovery.json"),
    JSON.stringify({ supervisorEnabled: true }),
  );
  const expected = {
    ...record,
    stateDir: realpathSync(canonicalState),
    supervisorMode: true,
  };
  try {
    await serve(
      (_req, res) => res.end(JSON.stringify(expected)),
      async (origin) => {
        const result = await ensureBackend({
          resources: "/missing-desktop-assets",
          port: Number(new URL(origin).port),
          env: { CODEX_AGENTS_STATE_DIR: canonicalState },
        });
        assert.equal(result.supervisorMode, true);
        assert.equal(result.owned, false);
      },
    );
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test.each([true, false])(
  "a fresh backend restores saved values or keeps first-install defaults (saved=%s)",
  async (hasSavedEnvironment) => {
    const root = realpathSync(
      mkdtempSync(path.join(tmpdir(), "studio-launch-env-")),
    );
    const canonicalState = path.join(root, "state");
    mkdirSync(canonicalState);
    mkdirSync(path.join(root, "web/dist"), { recursive: true });
    writeFileSync(path.join(root, "web/dist/index.html"), "fixture");
    const runtime = path.join(root, "fixture-runtime");
    writeFileSync(
      runtime,
      `#!${process.execPath}
const http = require("node:http"), fs = require("node:fs"), path = require("node:path");
const state = process.env.CODEX_AGENTS_STATE_DIR;
fs.writeFileSync(path.join(state, "fixture.pid"), String(process.pid));
http.createServer((request, response) => response.end(JSON.stringify({
  application:"codex-agents", protocol:1, backendKind:"rust", backendBuild:process.env.CODEX_BACKEND_BUILD,
  pid:process.pid, stateDir:state,
  restartEnvironment:Object.fromEntries(["CODEX_HOME", "CODEX_CANVAS_CWD"].filter(key => key in process.env).map(key => [key, process.env[key]]))
}))).listen(Number(process.argv.at(-1)), "127.0.0.1");
`,
      { mode: 0o700 },
    );
    const portServer = createServer();
    await new Promise((resolve) => portServer.listen(0, "127.0.0.1", resolve));
    const port = portServer.address().port;
    await new Promise((resolve) => portServer.close(resolve));
    const saved = {
      version: 1,
      stateDir: canonicalState,
      supervisorEnabled: false,
      environment: {
        CODEX_CANVAS_CWD: "/saved-workspace",
        CODEX_BIN: "/usr/bin/true",
        PATH: process.env.PATH,
      },
      unsetEnvironment: ["CODEX_HOME"],
    };
    const config = path.join(canonicalState, "background-recovery.json");
    if (hasSavedEnvironment) writeFileSync(config, JSON.stringify(saved));
    const env = {
      ...process.env,
      CODEX_AGENTS_STATE_DIR: canonicalState,
      CODEX_AGENTS_PYTHON: "/nonexistent",
      CODEX_RUNTIME_SERVER: runtime,
      CODEX_HOME: "/foreign-command-executor",
      CODEX_CANVAS_CWD: "/attaching-desktop",
      CODEX_BIN: "/usr/bin/true",
    };
    try {
      const result = await ensureBackend({ resources: root, port, env });
      assert.equal(result.owned, true);
      assert.deepEqual(
        result.restartEnvironment,
        hasSavedEnvironment
          ? { CODEX_CANVAS_CWD: "/saved-workspace" }
          : {
              CODEX_CANVAS_CWD: "/attaching-desktop",
              CODEX_HOME: "/foreign-command-executor",
            },
      );
      assert.equal(env.CODEX_HOME, "/foreign-command-executor");
      if (hasSavedEnvironment)
        assert.deepEqual(JSON.parse(readFileSync(config)), saved);
    } finally {
      try {
        const pid = Number(
          readFileSync(path.join(canonicalState, "fixture.pid"), "utf8"),
        );
        process.kill(pid, "SIGTERM");
      } catch {}
      rmSync(root, { recursive: true, force: true });
    }
  },
);

test("the desktop may attach to a diagnosed fallback generation", async () => {
  const canonicalState = realpathSync(tmpdir());
  const fallback = {
    ...record,
    stateDir: canonicalState,
    supervisorMode: false,
    supervisorFallback: true,
  };
  await serve(
    (_req, res) => res.end(JSON.stringify(fallback)),
    async (origin) => {
      const result = await ensureBackend({
        resources: "/missing-desktop-assets",
        port: Number(new URL(origin).port),
        env: {
          CODEX_AGENTS_STATE_DIR: canonicalState,
          CODEX_AGENTS_SUPERVISOR_MODE: "1",
        },
      });
      assert.equal(result.supervisorFallback, true);
      assert.equal(result.owned, false);
    },
  );
});

test("desktop status hashes the Rust executable for match and mismatch decisions", () => {
  const env = { CODEX_RUNTIME_SERVER: process.execPath };
  const installed = backendBuild(repositoryRoot, env);
  assert.equal(
    installed,
    createHash("sha256").update(readFileSync(process.execPath)).digest("hex"),
  );
  assert.deepEqual(
    updateStatus(repositoryRoot, { backendBuild: installed }, env),
    {
      availableBackendBuild: installed,
      updateRequired: false,
    },
  );
  assert.deepEqual(
    updateStatus(repositoryRoot, { backendBuild: "a".repeat(64) }, env),
    {
      availableBackendBuild: installed,
      updateRequired: true,
    },
  );
});

test("an app restart reports a pending backend update and preserves the owner", async () => {
  const stateRoot = mkdtempSync(path.join(tmpdir(), "studio-backend-reuse-"));
  const canonicalState = realpathSync(stateRoot);
  const before = backendBuild(repositoryRoot, {
    CODEX_RUNTIME_SERVER: process.execPath,
  });
  try {
    for (const liveBuild of ["a".repeat(64), before]) {
      const data = {
        ...record,
        stateDir: canonicalState,
        backendBuild: liveBuild,
      };
      await serve(
        (req, res) => res.end(JSON.stringify(data)),
        async (origin) => {
          const result = await ensureBackend({
            resources: repositoryRoot,
            port: Number(new URL(origin).port),
            // No executable paths or built UI. Spawning would fail the check.
            env: {
              CODEX_AGENTS_STATE_DIR: canonicalState,
              CODEX_RUNTIME_SERVER: process.execPath,
            },
          });
          assert.equal(result.pid, process.pid);
          assert.equal(result.owned, false);
          assert.equal(result.availableBackendBuild, before);
          assert.equal(result.updateRequired, liveBuild !== before);
        },
      );
    }
  } finally {
    rmSync(stateRoot, { recursive: true, force: true });
  }
});

test("unavailable source cannot be reported as a current backend", () => {
  // Pin the server path: the checkout target/debug fallback may hold a build.
  const env = {
    CODEX_RUNTIME_SERVER: "/missing-desktop-source/bin/studio-runtime-server",
  };
  assert.deepEqual(updateStatus("/missing-desktop-source", record, env), {
    availableBackendBuild: null,
    updateRequired: null,
  });
});

test("Rust startup consumes the desktop identity and restart environment contracts", () => {
  const require = createRequire(import.meta.url);
  const {
    desktopProtocolVersion,
    backendBuildHexLength,
  } = require("./backend.cjs");
  const {
    restartKeys,
    retiredRestartKeys,
    restartEnvironmentVersion,
  } = require("./restart-environment.cjs");
  const rust = readFileSync(
    path.join(
      repositoryRoot,
      "workspaces/client/packages/client-core/src/startup.rs",
    ),
    "utf8",
  );
  const constant = (name) =>
    Number(rust.match(new RegExp(`const ${name}: \\w+ = (\\d+);`))?.[1]);
  assert.equal(constant("DESKTOP_PROTOCOL_VERSION"), desktopProtocolVersion);
  assert.equal(constant("BACKEND_BUILD_HEX_LENGTH"), backendBuildHexLength);
  assert.equal(
    constant("RESTART_ENVIRONMENT_VERSION"),
    restartEnvironmentVersion,
  );
  const keys = [
    ...rust
      .match(/const RESTART_KEYS:[\s\S]*?= &\[([\s\S]*?)\];/)[1]
      .matchAll(/"([^"]+)"/g),
  ].map((match) => match[1]);
  assert.deepEqual(keys, restartKeys);
  const retired = [
    ...rust
      .match(/const RETIRED_RESTART_KEYS:[\s\S]*?= &\[([\s\S]*?)\];/)[1]
      .matchAll(/"([^"]+)"/g),
  ].map((match) => match[1]);
  assert.deepEqual(retired, retiredRestartKeys);
});
