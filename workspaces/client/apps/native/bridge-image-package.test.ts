import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { promisify } from "node:util";
import { test } from "node:test";
import { pruneBridgeImageCodecs } from "./bridge-image-package.ts";

const run = promisify(execFile);
const repository = path.resolve(import.meta.dirname, "../../../..");
const source = path.join(repository, "workspaces/providers/apps/claude-bridge");

test("the Mac stage retains only its ARM64 codec pair and preserves guest manifests", async () => {
  const directory = await fs.mkdtemp(
    path.join(os.tmpdir(), "studio-image-package-"),
  );
  try {
    const manifest = await fs.readFile(
      path.join(source, "package.json"),
      "utf8",
    );
    await fs.writeFile(path.join(directory, "package.json"), manifest);
    for (const name of Object.keys(JSON.parse(manifest).dependencies))
      await fs.mkdir(path.join(directory, "node_modules", name), {
        recursive: true,
      });
    const result = await pruneBridgeImageCodecs(directory, {
      platform: "darwin",
      arch: "arm64",
    });
    assert.equal(result.removed.length, 4);
    for (const name of result.kept)
      await fs.access(path.join(directory, "node_modules", name));
    for (const name of result.removed)
      await assert.rejects(
        fs.access(path.join(directory, "node_modules", name)),
        { code: "ENOENT" },
      );
    await fs.access(path.join(directory, "node_modules/sharp"));
    assert.equal(
      await fs.readFile(path.join(directory, "package.json"), "utf8"),
      manifest,
    );
    await assert.rejects(
      pruneBridgeImageCodecs(directory, { platform: "darwin", arch: "x64" }),
      /no image codec/,
    );
  } finally {
    await fs.rm(directory, { recursive: true, force: true });
  }
});

test(
  "the production deploy loads sharp after the Mac codec prune",
  {
    skip: process.platform !== "darwin" || process.arch !== "arm64",
    timeout: 60_000,
  },
  async () => {
    const root = await fs.mkdtemp(
      path.join(os.tmpdir(), "studio-image-deploy-"),
    );
    const directory = path.join(root, "bridge");
    try {
      await run(
        "pnpm",
        [
          "--filter",
          "studio-claude-bridge",
          "--prod",
          "deploy",
          "--no-optional",
          "--ignore-scripts",
          "--node-linker=hoisted",
          directory,
        ],
        { cwd: repository, timeout: 45_000 },
      );
      await pruneBridgeImageCodecs(directory);
      const { stdout } = await run(
        process.execPath,
        [path.join(directory, "testing/image-codec-witness.mjs")],
        { cwd: root, timeout: 10_000 },
      );
      assert.deepEqual(JSON.parse(stdout), {
        platform: "darwin",
        arch: "arm64",
        sharp: JSON.parse(
          await fs.readFile(path.join(directory, "package.json"), "utf8"),
        ).dependencies.sharp,
        cases: 14,
        result: "passed",
      });
      for (const name of [
        "sharp-linux-arm64",
        "sharp-linux-x64",
        "sharp-libvips-linux-arm64",
        "sharp-libvips-linux-x64",
      ])
        await assert.rejects(
          fs.access(path.join(directory, "node_modules/@img", name)),
          { code: "ENOENT" },
        );
    } finally {
      await fs.rm(root, { recursive: true, force: true });
    }
  },
);
