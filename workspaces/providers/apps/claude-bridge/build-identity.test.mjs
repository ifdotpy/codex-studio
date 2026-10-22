import { test } from "vitest";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { once } from "node:events";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { createInterface } from "node:readline";
import { fileURLToPath } from "node:url";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const HANG_GUARD_MS = 10_000;

async function initializedBuild(root) {
  const child = spawn(
    process.execPath,
    [path.join(root, "bridge.mjs"), path.join(root, "state")],
    {
      cwd: root,
      stdio: ["pipe", "pipe", "pipe"],
      timeout: HANG_GUARD_MS,
    },
  );
  const exited = once(child, "exit");
  let stderr = "";
  child.stderr.on("data", (chunk) => (stderr += chunk));
  const lines = createInterface({ input: child.stdout });
  try {
    child.stdin.write(
      JSON.stringify({ id: 1, method: "initialize", params: {} }) + "\n",
    );
    for await (const line of lines) {
      const response = JSON.parse(line);
      if (response.id !== 1) continue;
      assert.equal(response.error, undefined);
      return response.result.capabilities.claudeBuild;
    }
    assert.fail("bridge exited before initialize: " + stderr);
  } finally {
    lines.close();
    child.kill();
    await exited;
  }
}

test("initialize identity changes when only the installed SDK version changes", async () => {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), "claude-build-"));
  try {
    for (const name of await fs.readdir(HERE)) {
      if (name.endsWith(".mjs") && !name.endsWith(".test.mjs"))
        await fs.copyFile(path.join(HERE, name), path.join(root, name));
    }
    for (const name of ["zod", "@modelcontextprotocol/sdk"]) {
      const destination = path.join(root, "node_modules", name);
      await fs.mkdir(path.dirname(destination), { recursive: true });
      await fs.symlink(path.join(HERE, "node_modules", name), destination);
    }
    const sdk = path.join(root, "node_modules/@anthropic-ai/claude-agent-sdk");
    await fs.mkdir(sdk, { recursive: true });
    await fs.symlink(
      path.join(HERE, "node_modules/@anthropic-ai/claude-agent-sdk/sdk.mjs"),
      path.join(sdk, "sdk.mjs"),
    );
    const manifest = path.join(sdk, "package.json");
    const writeVersion = (version, extra = {}) =>
      fs.writeFile(
        manifest,
        JSON.stringify({
          name: "@anthropic-ai/claude-agent-sdk",
          type: "module",
          exports: "./sdk.mjs",
          version,
          ...extra,
        }),
      );
    await writeVersion("1.0.0");
    const before = await initializedBuild(root);
    assert.equal(
      await initializedBuild(root),
      before,
      "unchanged installation is stable",
    );
    await writeVersion("1.0.1");
    const after = await initializedBuild(root);
    assert.notEqual(
      after,
      before,
      "SDK-only updates must invalidate retained bridges",
    );
    await writeVersion("1.0.1", { description: "unrelated metadata" });
    assert.equal(
      await initializedBuild(root),
      after,
      "only the SDK version affects identity",
    );
  } finally {
    await fs.rm(root, { recursive: true, force: true });
  }
});
