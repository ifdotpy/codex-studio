import assert from "node:assert/strict";
import {
  chmod,
  copyFile,
  mkdir,
  mkdtemp,
  rm,
  symlink,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { once } from "node:events";
import { delimiter, join } from "node:path";
import test from "node:test";
import type { BackendStartupMode } from "./backend-startup.child.mts";
import { startRustBackend } from "./rust-ui-fixture.ts";

async function withBackendScript(
  mode: BackendStartupMode,
  check: (state: string) => Promise<void>,
) {
  const root = await mkdtemp(join(tmpdir(), "rust-startup-exit-"));
  const binary = join(root, "backend.mts");
  const previous = process.env.CODEX_STUDIO_RUNTIME_SERVER;
  const previousMode = process.env.FIXTURE_BACKEND_MODE;
  try {
    await copyFile(
      new URL("./backend-startup.child.mts", import.meta.url),
      binary,
    );
    await chmod(binary, 0o700);
    process.env.FIXTURE_BACKEND_MODE = mode;
    process.env.CODEX_STUDIO_RUNTIME_SERVER = binary;
    await check(join(root, "state"));
  } finally {
    if (previous === undefined) delete process.env.CODEX_STUDIO_RUNTIME_SERVER;
    else process.env.CODEX_STUDIO_RUNTIME_SERVER = previous;
    if (previousMode === undefined) delete process.env.FIXTURE_BACKEND_MODE;
    else process.env.FIXTURE_BACKEND_MODE = previousMode;
    await rm(root, { recursive: true, force: true });
  }
}

test(
  "backend startup preserves a failing exit code",
  { timeout: 5000 },
  async () => {
    await withBackendScript("exit", async (state) => {
      await assert.rejects(
        startRustBackend(state),
        /studio-runtime-server fixture exited.*code 7/,
      );
    });
  },
);

test(
  "backend startup reports a signal instead of waiting for readiness timeout",
  { timeout: 5000, skip: process.platform === "win32" },
  async () => {
    await withBackendScript("signal", async (state) => {
      await assert.rejects(
        startRustBackend(state),
        /studio-runtime-server fixture exited.*signal SIGTERM/,
      );
    });
  },
);

test(
  "the scripted provider finds node when the host's codex shares its folder",
  { timeout: 5000, skip: process.platform === "win32" },
  async () => {
    // Homebrew installs `node` and `codex` in one folder; the fixture hides
    // that folder from the server, and the provider starts through `env node`.
    const host = await mkdtemp(join(tmpdir(), "rust-fixture-host-bin-"));
    const previous = process.env.PATH;
    try {
      await mkdir(host, { recursive: true });
      await writeFile(join(host, "codex"), "", {
        mode: 0o700,
      });
      await symlink(process.execPath, join(host, "node"));
      process.env.PATH = [host, "/usr/bin", "/bin"].join(delimiter);
      // Exit 0 only when node starts through env and no PATH folder has codex.
      await withBackendScript("path", async (state) => {
        await assert.rejects(
          startRustBackend(state),
          /studio-runtime-server fixture exited.*code 0/,
        );
      });
    } finally {
      process.env.PATH = previous;
      await rm(host, { recursive: true, force: true });
    }
  },
);

test(
  "backend startup uses the bound port announcement without HTTP polling",
  { timeout: 5000 },
  async () => {
    await withBackendScript("http", async (state) => {
      const backend = await startRustBackend(state);
      try {
        assert.equal(backend.origin, `http://127.0.0.1:${backend.port}`);
        assert.equal(backend.token, "fixture-token");
      } finally {
        const closed = once(backend.child, "close");
        backend.child.kill();
        await closed;
      }
    });
  },
);
