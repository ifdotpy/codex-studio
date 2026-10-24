import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";
import {
  deliveryPlan,
  deliveryPython,
  parseDeliveryArguments,
} from "../../../../runtime/apps/runtime-server/deliver.mjs";

const repository = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../../../../..",
);

test("Linux keeps its existing delivery owner and exact commit argument", () => {
  const plan = deliveryPlan({
    platform: "linux",
    commit: "a".repeat(40),
    repository,
  });
  assert.equal(plan.length, 1);
  assert.match(plan[0].args[0], /runtime-server\/deliver\.sh$/);
  assert.equal(plan[0].args[1], "a".repeat(40));
});

test("Mac runs the canary before the only live write through the recovery owner", () => {
  const options = parseDeliveryArguments([
    "HEAD",
    "--app",
    "/tmp/checked/Codex Studio.app",
  ]);
  const plan = deliveryPlan({
    ...options,
    platform: "darwin",
    commit: "b".repeat(40),
    repository,
  });
  assert.deepEqual(
    plan.map((step) => step.label),
    ["verify target", "upgrade canary", "stage and verify"],
  );
  assert.ok(plan[1].args.includes("upgrade-canary"));
  assert.ok(
    plan[2].args.some((argument) => argument.endsWith("deliver_server.py")),
  );
  assert.ok(
    plan.every(
      (step) => !step.args.includes("kill") && !step.args.includes("launchctl"),
    ),
  );
});

test("Mac dry run cannot build, start a canary, or stage an update", () => {
  assert.throws(() => parseDeliveryArguments(["HEAD", "--dry-run"]), /--app/);
  const options = parseDeliveryArguments([
    "HEAD",
    "--app",
    "/tmp/checked/Codex Studio.app",
    "--dry-run",
  ]);
  const plan = deliveryPlan({
    ...options,
    platform: "darwin",
    commit: "b".repeat(40),
    repository,
  });
  assert.equal(plan.length, 1);
  assert.ok(plan[0].args.includes("--dry-run"));
});

test("a Mac build targets its exact commit before the canary and staging", () => {
  const plan = deliveryPlan({
    ...parseDeliveryArguments(["HEAD"]),
    platform: "darwin",
    commit: "b".repeat(40),
    repository,
  });
  assert.deepEqual(
    plan.map((step) => step.label),
    ["package", "verify target", "upgrade canary", "stage and verify"],
  );
  assert.ok(plan[0].args.includes("package-native"));
});

test("unsupported platforms and unknown options cannot reach a delivery owner", () => {
  assert.throws(
    () =>
      deliveryPlan({ platform: "win32", commit: "a".repeat(40), repository }),
    /platform/,
  );
  assert.throws(() => parseDeliveryArguments(["HEAD", "--force"]), /Unknown/);
});

test("Mac staging boundary tests use disposable bundles and state", async () => {
  const result = execFileSync(
    await deliveryPython(),
    ["-I", "-B", "workspaces/client/apps/desktop/delivery-test.py"],
    { cwd: repository, encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] },
  );
  assert.equal(result, "");
});

test("the Mac delivery entry point works under Python isolated mode", async () => {
  const output = execFileSync(
    await deliveryPython(),
    ["-I", "-B", "workspaces/client/apps/desktop/deliver_server.py", "--help"],
    { cwd: repository, encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] },
  );
  assert.ok(output.includes("--commit"));
});
