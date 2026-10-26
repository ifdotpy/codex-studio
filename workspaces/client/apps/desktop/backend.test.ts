import { parsedRecord } from "./test-support.ts";
import { desktopProtocolVersion, backendBuildHexLength } from "./backend.ts";
import {
  restartKeys,
  retiredRestartKeys,
  restartEnvironmentVersion,
} from "./restart-environment.ts";
import { portOf, present, nodeFixture } from "./test-support.ts";
import assert from "node:assert/strict";
import { createServer } from "node:http";
import { afterEach, test, vi } from "vitest";
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
import {
  identity,
  ensureBackend,
  updateStatus,
  backendExitStatus,
} from "./backend.ts";
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

afterEach(() => {
  vi.clearAllTimers();
  vi.useRealTimers();
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

function virtualTimeouts() {
  vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "performance"] });
  // Node's native AbortSignal.timeout does not follow Vitest's fake clock.
  vi.spyOn(AbortSignal, "timeout").mockImplementation((ms) => {
    const controller = new AbortController();
    setTimeout(
      () => controller.abort(new DOMException("Timed out", "TimeoutError")),
      ms,
    );
    return controller.signal;
  });
}
const aborted = (signal: AbortSignal | null | undefined) =>
  new Promise<never>((_, reject) => {
    const activeSignal = present(signal);
    activeSignal.addEventListener("abort", () => reject(activeSignal.reason), {
      once: true,
    });
  });

test("desktop recognizes backend exit by signal during startup", () => {
  assert.equal(
    backendExitStatus({ exitCode: null, signalCode: "SIGTERM" }),
    "SIGTERM",
  );
  assert.equal(backendExitStatus({ exitCode: 23, signalCode: null }), 23);
  assert.equal(backendExitStatus({ exitCode: null, signalCode: null }), null);
});

async function serve(
  handler: import("node:http").RequestListener,
  run: (origin: string) => Promise<void>,
) {
  const server = createServer(handler);
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  try {
    await run(`http://127.0.0.1:${portOf(server)}`);
  } finally {
    server.closeAllConnections();
    await new Promise<void>((resolve) => server.close(() => resolve()));
  }
}

test.each(["headers", "body"])(
  "retry a timeout while reading identity %s",
  async (phase) => {
    virtualTimeouts();
    const fetch = vi.fn(async (_url: unknown, { signal }: RequestInit) => {
      if (fetch.mock.calls.length === 1) {
        if (phase === "headers") return aborted(signal);
        return { ok: true, json: () => aborted(signal) };
      }
      return Response.json(record);
    });
    vi.stubGlobal("fetch", fetch);
    const pending = identity("http://backend.invalid", state, timing);
    await vi.advanceTimersByTimeAsync(
      timing.attemptTimeoutMs + timing.retryDelayMs,
    );
    assert.deepEqual(await pending, record);
    assert.equal(fetch.mock.calls.length, 2);
  },
);

test("a server that never responds reaches a bounded failure", async () => {
  virtualTimeouts();
  vi.stubGlobal("fetch", async (_url: unknown, { signal }: RequestInit) =>
    aborted(signal),
  );
  const pending = assert.rejects(
    identity("http://backend.invalid", state, timing),
    /No replacement backend was started/,
  );
  await vi.advanceTimersByTimeAsync(timing.timeoutMs - 1);
  await vi.advanceTimersByTimeAsync(1);
  await pending;
});

test("a refusal after a timeout cannot authorize a replacement", async () => {
  virtualTimeouts();
  let calls = 0;
  vi.stubGlobal("fetch", async (_url: unknown, { signal }: RequestInit) => {
    if (++calls > 1)
      throw new TypeError("fetch failed", { cause: { code: "ECONNREFUSED" } });
    return aborted(signal);
  });
  const pending = assert.rejects(
    identity("http://backend.invalid", state, timing),
    /No replacement backend was started/,
  );
  await vi.advanceTimersByTimeAsync(timing.timeoutMs);
  await pending;
  assert.ok(calls > 1);
});

test.each([
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
])("incompatible identity %s fails without retries", async (body) => {
  const fetch = vi.fn(async () => new Response(body));
  vi.stubGlobal("fetch", fetch);
  await assert.rejects(
    identity("http://backend.invalid", state, timing),
    /incompatible/,
  );
  assert.equal(fetch.mock.calls.length, 1);
});

test("a slow existing backend attaches without spawning a process", async () => {
  const canonicalState = realpathSync(tmpdir());
  const data = { ...record, stateDir: canonicalState };
  virtualTimeouts();
  let requestedReady = () => {};
  const requested = new Promise<void>((resolve) => {
    requestedReady = resolve;
  });
  let reply = () => {};
  await serve(
    (_req, res) => {
      reply = () => res.end(JSON.stringify(data));
      requestedReady();
    },
    async (origin) => {
      const pending = ensureBackend({
        resources: "/missing-desktop-assets",
        port: Number(new URL(origin).port),
        env: {
          CODEX_AGENTS_STATE_DIR: canonicalState,
          CODEX_RUNTIME_SERVER:
            "/missing-desktop-assets/bin/studio-runtime-server",
        },
      });
      await requested;
      await vi.advanceTimersByTimeAsync(1200);
      reply();
      const result = await pending;
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
    writeFileSync(runtime, nodeFixture("runtime-environment"), { mode: 0o700 });
    const portServer = createServer();
    await new Promise<void>((resolve) =>
      portServer.listen(0, "127.0.0.1", resolve),
    );
    const port = portOf(portServer);
    await new Promise<void>((resolve) => portServer.close(() => resolve()));
    const saved = {
      version: 1,
      stateDir: canonicalState,
      supervisorEnabled: false,
      environment: {
        CODEX_CANVAS_CWD: "/saved-workspace",
        CODEX_BIN: process.execPath,
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
      CODEX_BIN: process.execPath,
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
        assert.deepEqual(parsedRecord(readFileSync(config, "utf8")), saved);
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

// SHA-256 of the tiny executable below, independent of the host Node binary.
const fixtureBuild =
  "306c6ca7407560340797866e077e053627ad409277d1b9da58106fce4cf717cb";

test("an app restart reports a pending backend update and preserves the owner", async () => {
  const stateRoot = mkdtempSync(path.join(tmpdir(), "studio-backend-reuse-"));
  const canonicalState = realpathSync(stateRoot);
  const runtime = path.join(stateRoot, "runtime");
  writeFileSync(runtime, "#!/bin/sh\nexit 0\n", { mode: 0o700 });
  try {
    for (const liveBuild of ["a".repeat(64), fixtureBuild]) {
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
            // No built UI: spawning would fail the check.
            env: {
              CODEX_AGENTS_STATE_DIR: canonicalState,
              CODEX_RUNTIME_SERVER: runtime,
            },
          });
          assert.equal(result.pid, process.pid);
          assert.equal(result.owned, false);
          assert.equal(result.availableBackendBuild, fixtureBuild);
          assert.equal(result.updateRequired, liveBuild !== fixtureBuild);
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
  const rust = readFileSync(
    path.join(
      repositoryRoot,
      "workspaces/client/packages/client-core/src/startup.rs",
    ),
    "utf8",
  );
  const constant = (name: string) =>
    Number(rust.match(new RegExp(`const ${name}: \\w+ = (\\d+);`))?.[1]);
  assert.equal(constant("DESKTOP_PROTOCOL_VERSION"), desktopProtocolVersion);
  assert.equal(constant("BACKEND_BUILD_HEX_LENGTH"), backendBuildHexLength);
  assert.equal(
    constant("RESTART_ENVIRONMENT_VERSION"),
    restartEnvironmentVersion,
  );
  const keys = [
    ...present(
      present(rust.match(/const RESTART_KEYS:[\s\S]*?= &\[([\s\S]*?)\];/))[1],
    ).matchAll(/"([^"]+)"/g),
  ].map((match) => match[1]);
  assert.deepEqual(keys, restartKeys);
  const retired = [
    ...present(
      present(
        rust.match(/const RETIRED_RESTART_KEYS:[\s\S]*?= &\[([\s\S]*?)\];/),
      )[1],
    ).matchAll(/"([^"]+)"/g),
  ].map((match) => match[1]);
  assert.deepEqual(retired, retiredRestartKeys);
});
