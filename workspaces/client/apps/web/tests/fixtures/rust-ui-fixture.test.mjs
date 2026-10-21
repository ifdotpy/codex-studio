import assert from "node:assert/strict";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { startRustBackend } from "./rust-ui-fixture.mjs";

async function withBackendScript(body, check) {
  const root = await mkdtemp(join(tmpdir(), "rust-startup-exit-"));
  const binary = join(root, "backend");
  const previous = process.env.CODEX_STUDIO_RUNTIME_SERVER;
  try {
    await writeFile(binary, `#!${process.execPath}\n${body}\n`, {
      mode: 0o700,
    });
    process.env.CODEX_STUDIO_RUNTIME_SERVER = binary;
    await check(join(root, "state"));
  } finally {
    if (previous === undefined) delete process.env.CODEX_STUDIO_RUNTIME_SERVER;
    else process.env.CODEX_STUDIO_RUNTIME_SERVER = previous;
    await rm(root, { recursive: true, force: true });
  }
}

test(
  "backend startup preserves a failing exit code",
  { timeout: 5000 },
  async () => {
    await withBackendScript("process.exit(7);", async (state) => {
      await assert.rejects(
        startRustBackend(state),
        /studio-runtime-server exited 7/,
      );
    });
  },
);

test(
  "backend startup reports a signal instead of waiting for readiness timeout",
  { timeout: 5000, skip: process.platform === "win32" },
  async () => {
    await withBackendScript(
      'process.kill(process.pid, "SIGTERM");',
      async (state) => {
        await assert.rejects(
          startRustBackend(state),
          /studio-runtime-server exited SIGTERM/,
        );
      },
    );
  },
);
