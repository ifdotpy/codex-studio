import assert from "node:assert/strict";
import { createServer } from "node:http";
import { test } from "node:test";
import {
  realpathSync,
  mkdtempSync,
  mkdirSync,
  writeFileSync,
  rmSync,
  copyFileSync,
  symlinkSync,
} from "node:fs";
import path from "node:path";
import { execFileSync, spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { tmpdir } from "node:os";
import { createRequire } from "node:module";
const { identity, ensureBackend, backendBuild, updateStatus } = createRequire(
  import.meta.url,
)("./backend.cjs");
const state = "/unused-studio-state";
const record = {
  application: "codex-agents",
  protocol: 1,
  backendBuild: "b".repeat(64),
  pid: process.pid,
  stateDir: state,
};
const timing = { attemptTimeoutMs: 50, timeoutMs: 250, retryDelayMs: 10 };

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
        env: { CODEX_AGENTS_STATE_DIR: canonicalState },
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

function sourceFixture(run) {
  const root = mkdtempSync(path.join(tmpdir(), "studio-backend-identity-"));
  mkdirSync(path.join(root, "scripts"));
  writeFileSync(path.join(root, "scripts/codex-canvas"), "# entry point\n");
  writeFileSync(path.join(root, "scripts/runtime.py"), "VALUE = 1\n");
  writeFileSync(path.join(root, "scripts/codex_federation_crypto.mjs"), "VALUE = 1\n");
  try {
    return run(root);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
}

test("the backend build matches the Python source identity", () => {
  sourceFixture((root) => {
    const packageRoot = path.join(root, "scripts/analytics");
    mkdirSync(path.join(packageRoot, "tests"), { recursive: true });
    mkdirSync(path.join(packageRoot, "benchmarks"));
    mkdirSync(path.join(packageRoot, "__pycache__"));
    mkdirSync(path.join(packageRoot, "vendor"));
    mkdirSync(path.join(packageRoot, "venv"));
    writeFileSync(path.join(packageRoot, "__init__.py"), "\n");
    writeFileSync(path.join(packageRoot, "rollout_parser.py"), "VALUE = 1\n");
    for (const directory of [
      "tests",
      "benchmarks",
      "__pycache__",
      "vendor",
      "venv",
    ])
      writeFileSync(
        path.join(packageRoot, directory, "ignored.py"),
        "VALUE = 1\n",
      );
    const scripts = path.resolve(
      path.dirname(fileURLToPath(import.meta.url)),
      "../scripts",
    );
    const pythonBuild = () =>
      execFileSync(
        process.env.CODEX_AGENTS_PYTHON || "python3",
        [
          "-c",
          "import sys; sys.path.insert(0, sys.argv[1]); from codex_backend_identity import backend_build; print(backend_build(sys.argv[2]))",
          scripts,
          path.join(root, "scripts"),
        ],
        { encoding: "utf8" },
      ).trim();
    const initial = backendBuild(root);
    assert.equal(initial, pythonBuild());
    writeFileSync(path.join(root, "scripts/ignored.pyc"), "cache");
    writeFileSync(path.join(packageRoot, "tests/ignored.py"), "VALUE = 2\n");
    assert.equal(backendBuild(root), initial);
    assert.equal(pythonBuild(), initial);
    writeFileSync(path.join(packageRoot, "rollout_parser.py"), "VALUE = 2\n");
    assert.notEqual(backendBuild(root), initial);
    assert.equal(backendBuild(root), pythonBuild());
    const changedPythonBuild = backendBuild(root);
    writeFileSync(path.join(root, "scripts/codex_federation_crypto.mjs"), "VALUE = 2\n");
    assert.notEqual(backendBuild(root), changedPythonBuild);
    assert.equal(backendBuild(root), pythonBuild());
  });
});

test("backend source identity rejects package symlinks in both implementations", () => {
  sourceFixture((root) => {
    const packageRoot = path.join(root, "scripts/analytics");
    mkdirSync(packageRoot);
    writeFileSync(path.join(packageRoot, "__init__.py"), "\n");
    writeFileSync(path.join(packageRoot, "source.py"), "VALUE = 1\n");
    symlinkSync(
      path.join(packageRoot, "source.py"),
      path.join(packageRoot, "linked.py"),
    );
    assert.throws(() => backendBuild(root), /symlink/i);
    const scripts = path.resolve(
      path.dirname(fileURLToPath(import.meta.url)),
      "../scripts",
    );
    const result = spawnSync(
      process.env.CODEX_AGENTS_PYTHON || "python3",
      [
        "-c",
        "import sys; sys.path.insert(0, sys.argv[1]); from codex_backend_identity import backend_build; backend_build(sys.argv[2])",
        scripts,
        path.join(root, "scripts"),
      ],
      { encoding: "utf8" },
    );
    assert.notEqual(result.status, 0);
    assert.match(result.stderr, /symlink/i);
  });
});

test("an app restart reports a pending backend update and preserves the owner", async () => {
  const root = mkdtempSync(path.join(tmpdir(), "studio-backend-reuse-"));
  mkdirSync(path.join(root, "scripts"));
  writeFileSync(path.join(root, "scripts/codex-canvas"), "# source\n");
  const canonicalState = realpathSync(tmpdir());
  const before = backendBuild(root);
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
            resources: root,
            port: Number(new URL(origin).port),
            // No executable paths or built UI. Spawning would fail the check.
            env: { CODEX_AGENTS_STATE_DIR: canonicalState },
          });
          assert.equal(result.pid, process.pid);
          assert.equal(result.owned, false);
          assert.equal(result.availableBackendBuild, before);
          assert.equal(result.updateRequired, liveBuild !== before);
        },
      );
    }
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("unavailable source cannot be reported as a current backend", () => {
  assert.deepEqual(updateStatus("/missing-desktop-source", record), {
    availableBackendBuild: null,
    updateRequired: null,
  });
});

test("a source install does not rewrite the identity of an existing Python process", () => {
  sourceFixture((root) => {
    copyFileSync(
      path.resolve(
        path.dirname(fileURLToPath(import.meta.url)),
        "../scripts/codex_backend_identity.py",
      ),
      path.join(root, "scripts/codex_backend_identity.py"),
    );
    copyFileSync(
      path.resolve(
        path.dirname(fileURLToPath(import.meta.url)),
        "../scripts/codex_source_inventory.py",
      ),
      path.join(root, "scripts/codex_source_inventory.py"),
    );
    const initial = backendBuild(root);
    const result = JSON.parse(
      execFileSync(
        process.env.CODEX_AGENTS_PYTHON || "python3",
        [
          "-c",
          [
            "import sys, json; from pathlib import Path",
            "sys.path.insert(0, sys.argv[1])",
            "import codex_backend_identity as identity",
            "before = identity.BACKEND_BUILD",
            "Path(sys.argv[1], 'runtime.py').write_text('VALUE = 3\\n')",
            "print(json.dumps({'before': before, 'live': identity.BACKEND_BUILD, 'installed': identity.backend_build()}))",
          ].join("; "),
          path.join(root, "scripts"),
        ],
        { encoding: "utf8" },
      ),
    );
    assert.equal(result.before, initial);
    assert.equal(result.live, initial);
    assert.notEqual(result.installed, initial);
    assert.equal(result.installed, backendBuild(root));
  });
});
