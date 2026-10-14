import { afterEach, expect, test } from "vitest";
import {
  mkdtemp,
  mkdir,
  rm,
  writeFile,
  chmod,
  readFile,
} from "node:fs/promises";
import { createServer } from "node:net";
import { spawnSync } from "node:child_process";
import os from "node:os";
import path from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);
const desktopDirectory = path.dirname(fileURLToPath(import.meta.url));
const { ensureBackend, backendSelection } = require("./backend.cjs");
const fixtures = [];

afterEach(async () => {
  for (const fixture of fixtures.splice(0)) {
    if (fixture.pid) {
      try {
        process.kill(fixture.pid, "SIGTERM");
      } catch {}
    }
    await rm(fixture.root, { recursive: true, force: true });
  }
});

test("Python remains the default backend and Rust requires an explicit selection", () => {
  expect(backendSelection({})).toBe("python");
  expect(backendSelection({ CODEX_DESKTOP_BACKEND: "python" })).toBe("python");
  expect(backendSelection({ CODEX_DESKTOP_BACKEND: "rust" })).toBe("rust");
  expect(() => backendSelection({ CODEX_DESKTOP_BACKEND: "other" })).toThrow(
    "CODEX_DESKTOP_BACKEND",
  );
});

test("explicit Rust selection starts the configured executable with desktop identity", async () => {
  const root = await mkdtemp(
    path.join(os.tmpdir(), "codex-desktop-rust-selection-"),
  );
  const state = path.join(root, "state");
  const resources = path.join(root, "resources");
  const scripts = path.join(resources, "workspaces/runtime/apps/server/src");
  const renderer = path.join(resources, "workspaces/client/apps/web/dist");
  const binary = path.join(root, "fake-runtime-server");
  fixtures.push({ root });
  await mkdir(scripts, { recursive: true });
  await mkdir(renderer, { recursive: true });
  await writeFile(path.join(scripts, "codex-canvas"), "fixture");
  await writeFile(path.join(renderer, "index.html"), "fixture");
  await writeFile(
    binary,
    `#!/usr/bin/env node
const http = require("node:http");
const port = Number(process.argv[process.argv.indexOf("--port") + 1]);
http.createServer((request, response) => {
  if (request.url !== "/api/desktop") { response.writeHead(404).end(); return; }
  response.setHeader("content-type", "application/json");
  response.end(JSON.stringify({ application: "codex-agents", protocol: 1, backendKind: "rust", backendBuild: process.env.CODEX_BACKEND_BUILD, pid: process.pid, stateDir: process.env.CODEX_AGENTS_STATE_DIR }));
}).listen(port, "127.0.0.1");
`,
  );
  await chmod(binary, 0o755);
  const port = await new Promise((resolve, reject) => {
    const probe = createServer();
    probe.once("error", reject);
    probe.listen(0, "127.0.0.1", () => {
      const selected = probe.address().port;
      probe.close((error) => (error ? reject(error) : resolve(selected)));
    });
  });
  const backend = await ensureBackend({
    resources,
    port,
    env: {
      ...process.env,
      CODEX_DESKTOP_BACKEND: "rust",
      CODEX_RUNTIME_SERVER: binary,
      CODEX_AGENTS_STATE_DIR: state,
      CODEX_BIN: process.execPath,
      CODEX_DESKTOP_PORT: String(port),
      XDG_CACHE_HOME: path.join(root, "cache"),
    },
  });
  fixtures[fixtures.length - 1].pid = backend.pid;
  expect(backend.owned).toBe(true);
  expect(backend.backendBuild).toMatch(/^[a-f0-9]{64}$/);
  expect(backend.stateDir).toBe(state);
  expect(backend.origin).toBe(`http://127.0.0.1:${port}`);
  const marker = JSON.parse(
    await readFile(path.join(state, "rust-state-owner.json"), "utf8"),
  );
  expect(marker).toEqual({
    runtime: "rust",
    formatVersion: 1,
    backendBuild: backend.backendBuild,
  });
});

test("Python refuses the Rust-owned state marker unless an exact owner is allowlisted", async () => {
  const root = await mkdtemp(
    path.join(os.tmpdir(), "codex-python-rust-fence-"),
  );
  fixtures.push({ root });
  await writeFile(
    path.join(root, "rust-state-owner.json"),
    JSON.stringify({
      runtime: "rust",
      formatVersion: 1,
      backendBuild: "a".repeat(64),
    }),
  );
  const source = path.resolve(
    desktopDirectory,
    "../../../runtime/apps/server/src",
  );
  const result = spawnSync(
    process.env.CODEX_AGENTS_PYTHON || "python3",
    ["-c", "from codex_state import state_dir; state_dir()"],
    {
      encoding: "utf8",
      env: {
        ...process.env,
        CODEX_AGENTS_STATE_DIR: root,
        PYTHONPATH: source,
      },
    },
  );
  expect(result.status).not.toBe(0);
  expect(result.stderr).toContain("Python is fenced from Rust-owned state");
});
