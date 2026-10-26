import assert from "node:assert/strict";
import { createServer } from "node:http";
import { spawn } from "node:child_process";
import { test } from "vitest";
import {
  realpathSync,
  mkdtempSync,
  mkdirSync,
  writeFileSync,
  readFileSync,
  rmSync,
  copyFileSync,
} from "node:fs";
import path from "node:path";
import { execFileSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { tmpdir } from "node:os";
import { createRequire } from "node:module";
import { createHash } from "node:crypto";
const {
  identity,
  ensureBackend,
  backendBuild,
  updateStatus,
  apiPython,
  backendExitStatus,
} = createRequire(import.meta.url)("./backend.cjs");
const repositoryRoot = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../../../..",
);
const serverSource = path.join(
  repositoryRoot,
  "workspaces/runtime/apps/server/src",
);

const state = "/unused-studio-state";
const record = {
  application: "codex-agents",
  protocol: 1,
  backendBuild: "b".repeat(64),
  pid: process.pid,
  stateDir: state,
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

test("desktop honors an equipped explicit API interpreter", () => {
  const root = mkdtempSync(path.join(tmpdir(), "studio-api-python-"));
  const python = path.join(root, "python");
  try {
    writeFileSync(python, "#!/bin/sh\nexit 0\n", { mode: 0o755 });
    assert.equal(apiPython(root, { CODEX_AGENTS_PYTHON: python }), python);
    writeFileSync(python, "#!/bin/sh\nexit 1\n", { mode: 0o755 });
    assert.throws(
      () => apiPython(root, { CODEX_AGENTS_PYTHON: python }),
      /CODEX_AGENTS_PYTHON/,
    );
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("desktop resolves the user-cache API environment by lock digest", () => {
  const root = mkdtempSync(path.join(tmpdir(), "studio-api-python-cache-"));
  const requirements = "fastapi==1\n";
  const digest = createHash("sha256").update(requirements).digest("hex");
  const python = path.join(
    root,
    "cache/codex-agents/python",
    digest,
    "bin/python",
  );
  try {
    writeFileSync(path.join(root, "requirements.txt"), requirements);
    mkdirSync(path.dirname(python), { recursive: true });
    writeFileSync(python, "#!/bin/sh\nexit 0\n", { mode: 0o755 });
    assert.equal(
      apiPython(root, { XDG_CACHE_HOME: path.join(root, "cache") }),
      python,
    );
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
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
    mkdirSync(path.join(root, "scripts"));
    mkdirSync(path.join(root, "web/dist"), { recursive: true });
    writeFileSync(path.join(root, "web/dist/index.html"), "fixture");
    writeFileSync(
      path.join(root, "scripts/codex-canvas"),
      `
import http.server, json, os, sys
from pathlib import Path
state = os.environ["CODEX_AGENTS_STATE_DIR"]
Path(state, "fixture.pid").write_text(str(os.getpid()))
class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = json.dumps({"application": "codex-agents", "protocol": 1,
            "backendBuild": "b" * 64, "pid": os.getpid(), "stateDir": state,
            "restartEnvironment": {key: os.environ[key] for key in
                ("CODEX_HOME", "CODEX_CANVAS_CWD") if key in os.environ}}).encode()
        self.send_response(200)
        self.end_headers()
        self.wfile.write(body)
http.server.HTTPServer(("127.0.0.1", int(sys.argv[-1])), Handler).serve_forever()
`,
    );
    const portServer = createServer();
    await new Promise((resolve) => portServer.listen(0, "127.0.0.1", resolve));
    const port = portServer.address().port;
    await new Promise((resolve) => portServer.close(resolve));
    const python =
      process.env.CODEX_AGENTS_PYTHON ||
      execFileSync("python3", ["-c", "import sys; print(sys.executable)"], {
        encoding: "utf8",
      }).trim();
    const saved = {
      version: 1,
      stateDir: canonicalState,
      supervisorEnabled: false,
      environment: {
        CODEX_CANVAS_CWD: "/saved-workspace",
        CODEX_BIN: "/usr/bin/true",
        CODEX_AGENTS_PYTHON: python,
        PATH: process.env.PATH,
      },
      unsetEnvironment: ["CODEX_HOME"],
    };
    const config = path.join(canonicalState, "background-recovery.json");
    if (hasSavedEnvironment) writeFileSync(config, JSON.stringify(saved));
    const env = {
      ...process.env,
      CODEX_AGENTS_STATE_DIR: canonicalState,
      CODEX_AGENTS_PYTHON: python,
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

test("recovery and desktop attach to the same independently launched supervisor", async () => {
  const root = mkdtempSync(path.join(tmpdir(), "srr-"));
  const canonicalState = path.join(root, "state");
  mkdirSync(canonicalState);
  const portServer = createServer();
  await new Promise((resolve) => portServer.listen(0, "127.0.0.1", resolve));
  const port = portServer.address().port;
  await new Promise((resolve) => portServer.close(resolve));
  const resources = repositoryRoot;
  const python =
    process.env.CODEX_AGENTS_PYTHON ||
    execFileSync("python3", ["-c", "import sys; print(sys.executable)"], {
      encoding: "utf8",
    }).trim();
  const recoveryConfig = path.join(canonicalState, "background-recovery.json");
  writeFileSync(
    recoveryConfig,
    JSON.stringify({
      version: 1,
      enabled: true,
      supervisorEnabled: true,
      stateDir: canonicalState,
      resources,
      python,
      codex: "/usr/bin/true",
      port,
      environment: {},
      unsetEnvironment: [],
    }),
  );
  let recovery;
  const recoveryOutput = [];
  let backend;
  let supervisor;
  try {
    // Model launchd starting the independent owner before either backend path.
    spawn(
      python,
      [
        "-B",
        path.join(serverSource, "codex_process_supervisor.py"),
        "--state",
        canonicalState,
      ],
      {
        stdio: "ignore",
        env: {
          ...process.env,
          CODEX_AGENTS_STATE_DIR: canonicalState,
          CODEX_AGENTS_SUPERVISOR_MODE: "1",
        },
      },
    );
    recovery = spawn(python, [
      "-B",
      path.join(
        repositoryRoot,
        "workspaces/client/apps/desktop/recover_backend.py",
      ),
      "--config",
      recoveryConfig,
    ]);
    recovery.stdout.on("data", (chunk) =>
      recoveryOutput.push(chunk.toString()),
    );
    recovery.stderr.on("data", (chunk) =>
      recoveryOutput.push(chunk.toString()),
    );
    try {
      const deadline = Date.now() + 15000;
      let recoveredBackend;
      while (Date.now() < deadline && !recoveredBackend) {
        recoveredBackend = await identity(
          `http://127.0.0.1:${port}`,
          realpathSync(canonicalState),
        );
        if (!recoveredBackend)
          await new Promise((resolve) => setTimeout(resolve, 50));
      }
      assert.ok(
        recoveredBackend,
        `Recovery did not start the backend: ${recoveryOutput.join("")}`,
      );
      backend = await ensureBackend({
        resources,
        port,
        env: {
          CODEX_AGENTS_STATE_DIR: canonicalState,
          CODEX_BIN: "/usr/bin/true",
          PATH: process.env.PATH,
        },
      });
    } catch (error) {
      let supervisorLog = "";
      let canvasLog = "";
      try {
        supervisorLog = readFileSync(
          path.join(canonicalState, "supervisor.log"),
          "utf8",
        );
      } catch {}
      try {
        canvasLog = readFileSync(
          path.join(canonicalState, "canvas.log"),
          "utf8",
        );
      } catch {}
      throw new Error(
        `${error.message}\nRecovery service: ${recoveryOutput.join("")}\nSupervisor: ${supervisorLog}\nBackend: ${canvasLog}`,
      );
    }
    assert.equal(backend.supervisorMode, true);
    assert.equal(backend.stateDir, realpathSync(canonicalState));
    assert.equal(recovery.exitCode, null);
    const health = JSON.parse(
      execFileSync(
        python,
        [
          "-B",
          path.join(serverSource, "codex_process_supervisor.py"),
          "--status-json",
          "--state",
          canonicalState,
        ],
        {
          encoding: "utf8",
          env: { ...process.env, CODEX_AGENTS_STATE_DIR: canonicalState },
        },
      ),
    );
    assert.equal(health.stateDir, realpathSync(canonicalState));
    assert.equal(health.recovery.blocked, null);
  } finally {
    if (recovery) {
      recovery.kill("SIGTERM");
      await new Promise((resolve) => {
        if (recovery.exitCode !== null) return resolve();
        recovery.once("exit", resolve);
      });
    }
    let backendPid;
    try {
      const current = await identity(
        `http://127.0.0.1:${port}`,
        realpathSync(canonicalState),
      );
      if (current.stateDir === realpathSync(canonicalState)) {
        backendPid = current.pid;
        process.kill(current.pid, "SIGTERM");
      }
    } catch {}
    if (backendPid) {
      const deadline = Date.now() + 5000;
      while (Date.now() < deadline) {
        try {
          execFileSync("/bin/ps", ["-p", String(backendPid), "-o", "stat="], {
            stdio: "ignore",
          });
          await new Promise((resolve) => setTimeout(resolve, 50));
        } catch {
          break;
        }
      }
    }
    try {
      supervisor = JSON.parse(
        readFileSync(path.join(canonicalState, "supervisor.lock"), "utf8"),
      );
      process.kill(supervisor.pid, "SIGTERM");
    } catch {}
    if (supervisor?.pid) {
      const deadline = Date.now() + 5000;
      while (Date.now() < deadline) {
        try {
          execFileSync(
            "/bin/ps",
            ["-p", String(supervisor.pid), "-o", "stat="],
            {
              stdio: "ignore",
            },
          );
          await new Promise((resolve) => setTimeout(resolve, 50));
        } catch {
          break;
        }
      }
    }
    rmSync(root, {
      recursive: true,
      force: true,
      maxRetries: 20,
      retryDelay: 50,
    });
  }
  assert.equal(recovery?.exitCode, 0);
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

test("desktop status uses Python identity for match and mismatch decisions", () => {
  const installed = backendBuild(repositoryRoot);
  assert.deepEqual(updateStatus(repositoryRoot, { backendBuild: installed }), {
    availableBackendBuild: installed,
    updateRequired: false,
  });
  assert.deepEqual(
    updateStatus(repositoryRoot, { backendBuild: "a".repeat(64) }),
    { availableBackendBuild: installed, updateRequired: true },
  );
});

test("the installed identity equals Python inventory on the real source tree", () => {
  const installed = backendBuild(repositoryRoot);
  const pythonInventory = execFileSync(
    process.env.CODEX_AGENTS_PYTHON || "python3",
    [
      "-B",
      "-c",
      "import sys; sys.path.insert(0, sys.argv[1]); from codex_backend_identity import backend_build; print(backend_build(sys.argv[1]))",
      serverSource,
    ],
    { encoding: "utf8" },
  ).trim();
  assert.equal(installed, pythonInventory);
});

test("packaged Resources/workspace/scripts remains an identity source", () => {
  const root = mkdtempSync(path.join(tmpdir(), "studio-backend-package-"));
  const scripts = path.join(root, "workspace/scripts");
  mkdirSync(scripts, { recursive: true });
  try {
    for (const name of [
      "codex_backend_identity.py",
      "codex_source_inventory.py",
      "codex_layout.py",
    ])
      copyFileSync(path.join(serverSource, name), path.join(scripts, name));
    writeFileSync(path.join(scripts, "codex-canvas"), "# entry point\n");
    writeFileSync(path.join(scripts, "runtime.py"), "VALUE = 1\n");
    assert.match(backendBuild(path.join(root, "workspace")), /^[a-f0-9]{64}$/);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("identity helper failure leaves the update decision unknown", () => {
  const root = mkdtempSync(
    path.join(tmpdir(), "studio-backend-helper-failure-"),
  );
  const python = path.join(root, "python");
  try {
    writeFileSync(
      python,
      '#!/bin/sh\nif [ "$1" = "-c" ]; then exit 0; fi\necho helper failed >&2\nexit 1\n',
      { mode: 0o755 },
    );
    const errors = [];
    const originalError = console.error;
    console.error = (...values) => errors.push(values);
    try {
      assert.deepEqual(
        updateStatus(repositoryRoot, record, { CODEX_AGENTS_PYTHON: python }),
        { availableBackendBuild: null, updateRequired: null },
      );
    } finally {
      console.error = originalError;
    }
    assert.equal(errors.length, 1);
    assert.match(errors[0].join(" "), /helper failed/);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("an app restart reports a pending backend update and preserves the owner", async () => {
  const stateRoot = mkdtempSync(path.join(tmpdir(), "studio-backend-reuse-"));
  const canonicalState = realpathSync(stateRoot);
  const before = backendBuild(repositoryRoot);
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
    rmSync(stateRoot, { recursive: true, force: true });
  }
});

test("unavailable source cannot be reported as a current backend", () => {
  assert.deepEqual(updateStatus("/missing-desktop-source", record), {
    availableBackendBuild: null,
    updateRequired: null,
  });
});
