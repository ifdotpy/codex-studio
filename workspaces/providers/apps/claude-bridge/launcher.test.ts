import { test } from "vitest";
import assert from "node:assert/strict";
import { execFileSync, spawnSync } from "node:child_process";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const launcher = fileURLToPath(new URL("./bridge.mjs", import.meta.url));

test("both entrypoints refuse before importing TypeScript when support is absent", () => {
  for (const args of [[], ["--refresh-auth"]]) {
    const result = spawnSync(
      process.execPath,
      [
        "--input-type=module",
        "-e",
        `
      Object.defineProperty(process.features, "typescript", { value: false });
      process.argv = [process.execPath, ${JSON.stringify(launcher)}, ...${JSON.stringify(args)}];
      await import(${JSON.stringify(new URL("./bridge.mjs", import.meta.url).href)});
    `,
      ],
      { encoding: "utf8" },
    );
    assert.equal(result.status, 1);
    assert.match(
      result.stderr,
      /requires Node.js with built-in TypeScript support enabled/,
    );
    assert.equal(result.stdout, "");
    assert.doesNotMatch(
      result.stderr,
      /ERR_UNKNOWN_FILE_EXTENSION|Cannot find/,
    );
  }
});

test("the stable launcher selects the bridge and refresh TypeScript entrypoints", async () => {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), "bridge-launcher-"));
  try {
    await fs.copyFile(launcher, path.join(root, "bridge.mjs"));
    await fs.writeFile(
      path.join(root, "package.json"),
      JSON.stringify({ type: "module" }),
    );
    for (const name of ["bridge", "refresh-auth"]) {
      await fs.writeFile(
        path.join(root, `${name}.ts`),
        `const value: string = ${JSON.stringify(name)}; console.log(value);`,
      );
    }
    for (const [args, expected] of [
      [[], "bridge"],
      [["--refresh-auth"], "refresh-auth"],
    ] satisfies [string[], string][]) {
      assert.equal(
        execFileSync(
          process.execPath,
          [path.join(root, "bridge.mjs"), ...args],
          { encoding: "utf8" },
        ).trim(),
        expected,
      );
    }
  } finally {
    await fs.rm(root, { recursive: true, force: true });
  }
});
