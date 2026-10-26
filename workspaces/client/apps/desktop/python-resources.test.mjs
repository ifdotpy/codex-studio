import { copyFile, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { afterEach, expect, test } from "vitest";
import {
  missingPythonModules,
  pythonEntryPoints,
  pythonResources,
} from "./python-resources.ts";

const source = path.dirname(fileURLToPath(import.meta.url));
const directories = [];

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
  expect(missingPythonModules(resources)).toEqual([]);
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
