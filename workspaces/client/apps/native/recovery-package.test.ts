import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { access, mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { test } from "node:test";
import {
  nativeRecoveryResourceFiles,
  packageRecoveryHelpers,
} from "./recovery-package.ts";

test("the packaged recovery entry loads its TypeScript helper closure and preserves installation refusal", async () => {
  const resources = await mkdtemp(
    path.join(tmpdir(), "studio-native-recovery-"),
  );
  try {
    await packageRecoveryHelpers(resources);
    for (const file of nativeRecoveryResourceFiles)
      await access(path.join(resources, file));
    assert.deepEqual(
      JSON.parse(
        await readFile(path.join(resources, "helpers/package.json"), "utf8"),
      ),
      { type: "module" },
    );
    const run = spawnSync(
      process.execPath,
      [
        path.join(resources, "helpers/recovery.ts"),
        path.join(resources, "uninstalled-application"),
        "/Applications/Codex Studio.app/Contents/Resources",
        "false",
      ],
      {
        encoding: "utf8",
        timeout: 10_000,
        cwd: resources,
        env: {
          ...process.env,
          CODEX_AGENTS_STATE_DIR: path.join(resources, "state"),
        },
      },
    );
    assert.equal(run.status, 1, run.stderr);
    assert.match(run.stderr, /Background recovery can be registered only by/);
    assert.equal(run.stdout, "");
    await assert.rejects(access(path.join(resources, "state")), {
      code: "ENOENT",
    });
  } finally {
    await rm(resources, { recursive: true, force: true });
  }
});
