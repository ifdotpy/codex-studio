import {
  copyFile,
  mkdtemp,
  readFile,
  rm,
  unlink,
  writeFile,
} from "node:fs/promises";
import { execFileSync } from "node:child_process";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { afterEach, expect, test } from "vitest";
import {
  missingPythonModules,
  pythonEntryPoints,
  pythonResources,
} from "./python-resources.mjs";

const source = path.dirname(fileURLToPath(import.meta.url));
const directories = [];
const { supportedPython } = createRequire(import.meta.url)("./recovery.cjs");

async function directory() {
  const created = await mkdtemp(path.join(tmpdir(), "studio-python-"));
  directories.push(created);
  return created;
}

afterEach(async () => {
  for (const created of directories.splice(0))
    await rm(created, { recursive: true, force: true });
});

test("the packaged Python files supply every module that the entry points import", async () => {
  const resources = await directory();
  for (const file of pythonResources)
    await copyFile(path.join(source, file), path.join(resources, file));
  expect(pythonResources).toEqual(expect.arrayContaining(pythonEntryPoints));
  const contract = JSON.parse(
    await readFile(path.join(source, "delivery-contract.json"), "utf8"),
  );
  expect(pythonResources).toEqual(
    expect.arrayContaining(contract.startupResources),
  );
  expect(missingPythonModules(resources)).toEqual([]);
  const script = path.join(resources, "recover_backend.py");
  const python = await supportedPython({ env: process.env, saved: {}, script });
  await unlink(path.join(resources, "delivery-contract.json"));
  expect(() =>
    execFileSync(python, ["-B", script, "--check-interpreter"], {
      stdio: "pipe",
    }),
  ).toThrow();
});

test("a local module import that the directory does not supply is reported", async () => {
  const resources = await directory();
  await writeFile(
    path.join(resources, "entry.py"),
    "import json\n\ndef load():\n    import helper\n",
  );
  await writeFile(
    path.join(resources, "helper.py"),
    "from __future__ import annotations\nfrom pathlib import Path\nfrom absent_helper import VALUE\n",
  );
  expect(missingPythonModules(resources, ["entry.py"])).toEqual([
    "absent_helper",
  ]);
});
