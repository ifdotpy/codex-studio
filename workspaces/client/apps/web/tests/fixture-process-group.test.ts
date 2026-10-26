import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { mkdtempSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import {
  fixtureProcessGroupExists,
  reapFixtureProcessGroups,
  registerFixtureProcessGroup,
} from "./fixture-process-group.ts";

test("stopping a fixture process group reaps its descendant processes", async () => {
  if (process.platform === "win32") return;

  const registryDirectory = mkdtempSync(join(tmpdir(), "fixture-groups-"));
  const child = spawn(
    process.execPath,
    [
      "-e",
      `const { spawn } = require("node:child_process");
       const grandchild = spawn(process.execPath, ["-e", "setInterval(() => {}, 1000)"], { stdio: "ignore" });
       process.stdout.write(String(grandchild.pid));
       setInterval(() => {}, 1000);`,
    ],
    { detached: true, stdio: ["ignore", "pipe", "ignore"] },
  );
  assert.ok(child.pid !== undefined);
  const childPid = child.pid;
  try {
    const [output] = await once(child.stdout, "data");
    const grandchildPid = Number(String(output));
    assert.ok(Number.isSafeInteger(grandchildPid));
    assert.equal(fixtureProcessGroupExists(childPid), true);
    registerFixtureProcessGroup(registryDirectory, childPid);

    await reapFixtureProcessGroups(registryDirectory);

    assert.equal(fixtureProcessGroupExists(childPid), false);
    assert.throws(() => process.kill(grandchildPid, 0), { code: "ESRCH" });
    assert.deepEqual(readdirSync(registryDirectory), []);
  } finally {
    if (child.pid !== undefined && fixtureProcessGroupExists(childPid))
      await reapFixtureProcessGroups(registryDirectory);
    rmSync(registryDirectory, { recursive: true, force: true });
  }
});

test("reaper ignores a registry entry for a reused process id", async () => {
  if (process.platform !== "linux") return;

  const registryDirectory = mkdtempSync(
    join(tmpdir(), "fixture-groups-stale-"),
  );
  try {
    writeFileSync(
      join(registryDirectory, `${process.pid}.json`),
      JSON.stringify({ pid: process.pid, startTime: "not-this-process" }),
    );

    await reapFixtureProcessGroups(registryDirectory);

    assert.equal(process.kill(process.pid, 0), true);
    assert.deepEqual(readdirSync(registryDirectory), []);
  } finally {
    rmSync(registryDirectory, { recursive: true, force: true });
  }
});

test("reaper stops fixture descendants after the direct process exits", async () => {
  if (process.platform === "win32") return;

  const registryDirectory = mkdtempSync(
    join(tmpdir(), "fixture-groups-orphaned-descendant-"),
  );
  const child = spawn(
    process.execPath,
    [
      "-e",
      `const { spawn } = require("node:child_process");
       const grandchild = spawn(process.execPath, ["-e", "setInterval(() => {}, 1000)"], { stdio: "ignore" });
       grandchild.unref();
       process.stdout.write(String(grandchild.pid));`,
    ],
    { detached: true, stdio: ["ignore", "pipe", "ignore"] },
  );
  assert.ok(child.pid !== undefined);
  const childPid = child.pid;
  try {
    await once(child, "spawn");
    registerFixtureProcessGroup(registryDirectory, childPid);
    const [output] = await once(child.stdout, "data");
    const grandchildPid = Number(String(output));
    await once(child, "close");
    assert.equal(fixtureProcessGroupExists(childPid), true);

    await reapFixtureProcessGroups(registryDirectory);

    assert.equal(fixtureProcessGroupExists(childPid), false);
    assert.throws(() => process.kill(grandchildPid, 0), { code: "ESRCH" });
    assert.deepEqual(readdirSync(registryDirectory), []);
  } finally {
    if (child.pid !== undefined && fixtureProcessGroupExists(childPid))
      await reapFixtureProcessGroups(registryDirectory);
    rmSync(registryDirectory, { recursive: true, force: true });
  }
});
