import assert from "node:assert/strict";
import { mkdir, mkdtemp, rm, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { delimiter, join } from "node:path";
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
      await writeFile(join(host, "codex"), "#!/bin/sh\nexit 0\n", {
        mode: 0o700,
      });
      await symlink(process.execPath, join(host, "node"));
      process.env.PATH = [host, "/usr/bin", "/bin"].join(delimiter);
      // Exit 0 only when node starts through env and no PATH folder has codex.
      await withBackendScript(
        `const { existsSync } = require("node:fs");
const { join } = require("node:path");
const entries = process.env.PATH.split(${JSON.stringify(delimiter)});
const hidden = entries.every((entry) => !existsSync(join(entry, "codex")));
const node = require("node:child_process").spawnSync("/usr/bin/env", ["node", "-e", ""]);
process.exit(hidden && node.status === 0 ? 0 : 9);`,
        async (state) => {
          await assert.rejects(
            startRustBackend(state),
            /studio-runtime-server exited 0/,
          );
        },
      );
    } finally {
      process.env.PATH = previous;
      await rm(host, { recursive: true, force: true });
    }
  },
);
