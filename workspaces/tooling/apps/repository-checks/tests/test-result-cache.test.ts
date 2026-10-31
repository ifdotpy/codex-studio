import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import {
  SHARED_INPUTS,
  cachedStepKey,
  passedCrates,
  selectCrates,
  recordPass,
  reusedResult,
  stepKey,
} from "../test-result-cache.ts";
import type { GateStep } from "../facade.ts";

function repository() {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "test-result-cache-"));
  const git = (...args: string[]) => execFileSync("git", args, { cwd: root });
  git("init", "-q");
  git("config", "user.email", "fixture@example.invalid");
  git("config", "user.name", "Fixture");
  git("config", "commit.gpgsign", "false");
  for (const input of SHARED_INPUTS) {
    const file = input.includes(".") ? input : path.join(input, "owner.ts");
    fs.mkdirSync(path.dirname(path.join(root, file)), { recursive: true });
    fs.writeFileSync(path.join(root, file), `${input}\n`);
  }
  fs.mkdirSync(path.join(root, "guest"));
  fs.writeFileSync(path.join(root, "guest", "suite.py"), "one\n");
  fs.writeFileSync(path.join(root, "other.txt"), "one\n");
  git("add", "-A");
  git("commit", "-qm", "fixture");
  return { root, git };
}

const step = {
  id: "guest",
  inputs: ["guest"],
  commands: [["node", ["x"]]],
} satisfies Partial<GateStep>;
const passed = {
  status: "passed",
  exitCode: 0,
  durationSeconds: 0,
  passed: 3,
  executed: 3,
};

test("a passing step is reused until one of its inputs changes", () => {
  const { root, git } = repository();
  const key = stepKey(root, step, {});
  assert.ok(key);
  assert.equal(reusedResult(root, step, key), undefined);
  recordPass(root, step, key, passed);
  assert.deepEqual(reusedResult(root, step, stepKey(root, step, {})!), {
    ...passed,
    durationSeconds: 0,
    reused: true,
  });

  // A change outside the inputs keeps the result.
  fs.writeFileSync(path.join(root, "other.txt"), "two\n");
  git("commit", "-qam", "other");
  assert.equal(stepKey(root, step, {}), key);

  // An uncommitted input change makes a new key; the same content gives the same key again.
  fs.writeFileSync(path.join(root, "guest", "suite.py"), "two\n");
  const dirty = stepKey(root, step, {});
  assert.ok(dirty && dirty !== key);
  assert.equal(stepKey(root, step, {}), dirty);
  fs.writeFileSync(path.join(root, "guest", "suite.py"), "one\n");
  assert.equal(stepKey(root, step, {}), key);
  fs.writeFileSync(path.join(root, "guest", "suite.py"), "two\n");

  // A committed input change makes a new key without a saved result.
  git("commit", "-qam", "guest");
  const changed = stepKey(root, step, {});
  assert.ok(changed && changed !== key);
  assert.equal(reusedResult(root, step, changed), undefined);

  // A shared input change also invalidates the result.
  fs.writeFileSync(path.join(root, "pnpm-lock.yaml"), "two\n");
  git("commit", "-qam", "lock");
  assert.notEqual(stepKey(root, step, {}), changed);
});

test("failures are not saved and the switch turns reuse off", () => {
  const { root } = repository();
  const key = stepKey(root, step, {})!;
  recordPass(root, step, key, {
    status: "failed",
    exitCode: 1,
    durationSeconds: 0,
  });
  assert.equal(reusedResult(root, step, key), undefined);
  assert.equal(stepKey(root, step, { STUDIO_TEST_REUSE: "0" }), undefined);
  assert.equal(stepKey(root, { id: "no-inputs" }, {}), undefined);
});

test("a shared result folder carries a passing result to another checkout", () => {
  const first = repository();
  const second = repository();
  const shared = fs.mkdtempSync(path.join(os.tmpdir(), "test-results-shared-"));
  const env = { STUDIO_TEST_RESULTS_DIR: shared };
  const key = stepKey(first.root, step, env)!;
  assert.equal(stepKey(second.root, step, env), key);
  process.env.STUDIO_TEST_RESULTS_DIR = shared;
  try {
    recordPass(first.root, step, key, passed);
    assert.equal(reusedResult(second.root, step, key)?.reused, true);
  } finally {
    delete process.env.STUDIO_TEST_RESULTS_DIR;
  }
  assert.equal(reusedResult(second.root, step, key)?.reused, undefined);
  assert.equal(fs.existsSync(path.join(first.root, "target")), false);
});

test("a rerun on an unsaved tree keys on file content, including new and deleted files", () => {
  const { root } = repository();
  const key = stepKey(root, step, {})!;
  fs.writeFileSync(path.join(root, "guest", "new.py"), "new\n");
  const added = stepKey(root, step, {});
  assert.ok(added && added !== key);
  fs.rmSync(path.join(root, "guest", "new.py"));
  assert.equal(stepKey(root, step, {}), key);
  fs.rmSync(path.join(root, "guest", "suite.py"));
  const removed = stepKey(root, step, {});
  assert.ok(removed && removed !== key);
  // A file outside the inputs never changes the key.
  fs.writeFileSync(path.join(root, "unrelated.txt"), "x\n");
  assert.equal(stepKey(root, step, {}), removed);
});

test("a step without declared inputs is keyed by the whole tree, an artifact step never", () => {
  const { root } = repository();
  const plain = {
    id: "tooling",
    commands: [["node", ["x"]]],
  } satisfies Partial<GateStep>;
  const key = cachedStepKey(root, plain, {});
  assert.ok(key);
  assert.equal(cachedStepKey(root, plain, {}), key);
  fs.writeFileSync(path.join(root, "other.txt"), "two\n");
  assert.notEqual(cachedStepKey(root, plain, {}), key);
  assert.equal(
    cachedStepKey(root, { ...plain, provides: "workspace" }, {}),
    undefined,
  );
  assert.equal(
    cachedStepKey(root, plain, { STUDIO_TEST_REUSE: "0" }),
    undefined,
  );
});

test("passedCrates lists the crates of a complete nextest report without a failure", () => {
  const report = (
    total: number,
  ) => `<testsuites name="nextest-run" tests="${total}"><testsuite name="studio-a" tests="2" skipped="0" failures="0" errors="0"><testcase name="x"/></testsuite>
<testsuite name="studio-b::bin/studio-b" tests="1" skipped="0" failures="1" errors="0"><testcase name="y"><failure/></testcase></testsuite>
<testsuite name="studio-b::integration" tests="1" skipped="0" failures="0" errors="0"><testcase name="z"/></testsuite>
<testsuite name="studio-c" tests="1" skipped="0" failures="0" errors="1"><testcase name="w"><error/></testcase></testsuite>
<testsuite name="studio-d" tests="1" skipped="1" failures="0" errors="0"><testcase name="v"><skipped/></testcase></testsuite></testsuites>`;
  // The run planned 6 tests and reported 6: only crate studio-a has all tests passed.
  assert.deepEqual([...passedCrates(report(6), 6)], ["studio-a"]);
  // A run that stopped early reports fewer tests than planned: no crate counts as passed,
  // also not the crates that never ran or ran only partly.
  assert.deepEqual([...passedCrates(report(5), 6)], []);
  assert.deepEqual([...passedCrates("not a report", 6)], []);
  assert.deepEqual([...passedCrates(report(6), undefined)], []);
});
test("nextest selects stale crates with a filter so the workspace feature set and build stay the same", () => {
  const selected = selectCrates(
    [["cargo", ["nextest", "run", "--workspace", "--profile", "fast"]]],
    ["studio-a", "studio-b"],
  );
  assert.deepEqual(selected, [
    [
      "cargo",
      [
        "nextest",
        "run",
        "--workspace",
        "-E",
        "package(=studio-a) | package(=studio-b)",
        "--profile",
        "fast",
      ],
    ],
  ]);
  // Cargo test has no filter set: it takes package arguments.
  assert.deepEqual(
    selectCrates([["cargo", ["test", "--workspace"]]], ["studio-a"]),
    [["cargo", ["test", "-p", "studio-a"]]],
  );
});
