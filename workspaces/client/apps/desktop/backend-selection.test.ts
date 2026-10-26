import { portOf, present, nodeFixture } from "./test-support.ts";
import { afterEach, expect, test } from "vitest";
import {
  mkdtemp,
  mkdir,
  realpath,
  rm,
  writeFile,
  chmod,
  readFile,
} from "node:fs/promises";
import { createServer } from "node:net";
import { createServer as createHttpServer } from "node:http";
import { createHash } from "node:crypto";
import os from "node:os";
import path from "node:path";

import { ensureBackend, identity } from "./backend.ts";
const fixtures: { root: string; pid?: number }[] = [];

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

test("desktop identity rejects a Rust backend build that differs from the selected executable", async () => {
  const state = path.join(os.tmpdir(), "codex-desktop-identity-fixture");
  const server = createHttpServer((request, response) => {
    response.setHeader("content-type", "application/json");
    response.end(
      JSON.stringify({
        application: "codex-agents",
        protocol: 1,
        backendKind: "rust",
        backendBuild: "b".repeat(64),
        pid: process.pid,
        stateDir: state,
      }),
    );
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  try {
    await expect(
      identity(`http://127.0.0.1:${portOf(server)}`, state, {
        timeoutMs: 1000,
        expectedBackendBuild: "a".repeat(64),
      }),
    ).rejects.toThrow("does not match the selected executable");
  } finally {
    await new Promise<void>((resolve, reject) =>
      server.close((error) => (error ? reject(error) : resolve())),
    );
  }
});

test("desktop starts only the Rust executable with Python unavailable", async () => {
  // The backend reports its canonical state directory (macOS /var is /private/var).
  const root = await realpath(
    await mkdtemp(path.join(os.tmpdir(), "codex-desktop-rust-selection-")),
  );
  const state = path.join(root, "state");
  const resources = path.join(root, "resources");
  const renderer = path.join(resources, "workspaces/client/apps/web/dist");
  const binary = path.join(root, "fake-runtime-server");
  fixtures.push({ root });
  await mkdir(renderer, { recursive: true });
  await writeFile(path.join(renderer, "index.html"), "fixture");
  await writeFile(binary, nodeFixture("runtime-identity"));
  await chmod(binary, 0o755);
  const port = await new Promise<number>((resolve, reject) => {
    const probe = createServer();
    probe.once("error", reject);
    probe.listen(0, "127.0.0.1", () => {
      const selected = portOf(probe);
      probe.close((error) => (error ? reject(error) : resolve(selected)));
    });
  });
  const backend = await ensureBackend({
    resources,
    port,
    env: {
      ...process.env,
      CODEX_AGENTS_PYTHON: "/nonexistent",
      CODEX_RUNTIME_SERVER: binary,
      CODEX_AGENTS_STATE_DIR: state,
      CODEX_BIN: process.execPath,
      CODEX_DESKTOP_PORT: String(port),
      XDG_CACHE_HOME: path.join(root, "cache"),
    },
  });
  present(fixtures.at(-1)).pid = backend.pid;
  expect(backend.owned).toBe(true);
  expect(backend.backendBuild).toMatch(/^[a-f0-9]{64}$/);
  expect(backend.backendBuild).toBe(
    createHash("sha256")
      .update(await readFile(binary))
      .digest("hex"),
  );
  expect(backend.stateDir).toBe(state);
  expect(backend.origin).toBe(`http://127.0.0.1:${port}`);
  expect(await readFile(path.join(state, "canvas.pid"), "utf8")).toBe(
    `${backend.pid}\n`,
  );
  expect(await identity(backend.origin, state)).toMatchObject({
    application: "codex-agents",
    protocol: 1,
    backendBuild: backend.backendBuild,
    pid: backend.pid,
    stateDir: state,
  });
});
