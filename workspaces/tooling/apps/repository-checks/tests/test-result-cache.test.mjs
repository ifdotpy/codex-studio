import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import {
  SHARED_INPUTS,
  recordPass,
  reusedResult,
  stepKey,
} from "../test-result-cache.mjs";

function repository() {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "test-result-cache-"));
  const git = (...args) => execFileSync("git", args, { cwd: root });
  git("init", "-q");
  git("config", "user.email", "fixture@example.invalid");
  git("config", "user.name", "Fixture");
  git("config", "commit.gpgsign", "false");
  for (const input of SHARED_INPUTS) {
    const file = input.includes(".") ? input : path.join(input, "owner.mjs");
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

const step = { id: "guest", inputs: ["guest"], commands: [["node", ["x"]]] };
const passed = { status: "passed", exitCode: 0, passed: 3, executed: 3 };

test("a passing step is reused until one of its inputs changes", () => {
  const { root, git } = repository();
  const key = stepKey(root, step, {});
  assert.ok(key);
  assert.equal(reusedResult(root, step, key), undefined);
  recordPass(root, step, key, passed);
  assert.deepEqual(reusedResult(root, step, stepKey(root, step, {})), {
    ...passed,
    durationSeconds: 0,
    reused: true,
  });

  // A change outside the inputs keeps the result.
  fs.writeFileSync(path.join(root, "other.txt"), "two\n");
  git("commit", "-qam", "other");
  assert.equal(stepKey(root, step, {}), key);

  // An uncommitted input change always runs the step.
  fs.writeFileSync(path.join(root, "guest", "suite.py"), "two\n");
  assert.equal(stepKey(root, step, {}), undefined);

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
  const key = stepKey(root, step, {});
  recordPass(root, step, key, { status: "failed", exitCode: 1 });
  assert.equal(reusedResult(root, step, key), undefined);
  assert.equal(stepKey(root, step, { STUDIO_TEST_REUSE: "0" }), undefined);
  assert.equal(stepKey(root, { id: "no-inputs" }, {}), undefined);
});
