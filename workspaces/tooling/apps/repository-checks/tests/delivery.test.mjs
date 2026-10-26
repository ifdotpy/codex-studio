import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import path from "node:path";
import os from "node:os";
import fs from "node:fs";
import { fileURLToPath } from "node:url";
import test from "node:test";
import {
  deliveryPlan,
  deliveryPython,
  parseDeliveryArguments,
} from "../../../../runtime/apps/runtime-server/deliver.mjs";
import * as delivery from "../../../../runtime/apps/runtime-server/deliver.mjs";

const repository = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../../../../..",
);
const fixtureApp = path.join(os.tmpdir(), "checked", "Codex Studio.app");

test("installed-delivery recovery uses the existing route without selecting a backend PID", () => {
  const state = path.join(os.tmpdir(), "recovered-delivery-state");
  const prior = "4b1e1a51-3df8-555f-906b-4b1d685b5b52";
  const options = parseDeliveryArguments([
    "HEAD",
    "--app",
    fixtureApp,
    "--recover-installed-delivery",
    prior,
    "--state",
    state,
  ]);
  const plan = deliveryPlan({
    ...options,
    platform: "darwin",
    commit: "b".repeat(40),
    repository,
  });
  for (const step of plan.filter((item) =>
    ["verify target", "stage and verify"].includes(item.label),
  )) {
    assert.equal(
      step.args[step.args.indexOf("--recover-installed-delivery") + 1],
      prior,
    );
    assert.equal(step.args[step.args.indexOf("--state") + 1], state);
    assert.ok(!step.args.includes("--starting-backend-pid"));
  }
  for (const args of [
    ["--app", fixtureApp, "--recover-installed-delivery", prior],
    [
      "--app",
      fixtureApp,
      "--recover-installed-delivery",
      "invalid",
      "--state",
      state,
    ],
    [
      "--app",
      fixtureApp,
      "--recover-installed-delivery",
      prior,
      "--state",
      state,
      "--starting-backend-pid",
      "11",
    ],
  ])
    assert.throws(() => parseDeliveryArguments(args));
});

test("startup delivery pins one PID and canonical state through the existing staging owner", () => {
  const state = path.join(os.tmpdir(), "startup-delivery-state");
  const options = parseDeliveryArguments([
    "HEAD",
    "--app",
    fixtureApp,
    "--starting-backend-pid",
    "36156",
    "--state",
    state,
  ]);
  assert.equal(options.startingBackendPid, 36156);
  const plan = deliveryPlan({
    ...options,
    platform: "darwin",
    commit: "b".repeat(40),
    repository,
  });
  for (const step of plan.filter((item) =>
    ["verify target", "stage and verify"].includes(item.label),
  )) {
    assert.equal(
      step.args[step.args.indexOf("--starting-backend-pid") + 1],
      "36156",
    );
    assert.equal(step.args[step.args.indexOf("--state") + 1], state);
  }
  assert.equal(
    plan.filter((step) =>
      step.args?.some((arg) => arg.endsWith("deliver_server.py")),
    ).length,
    2,
  );
  assert.ok(
    !plan.some((step) =>
      step.args?.some((arg) => arg.endsWith("install_update.py")),
    ),
  );
  for (const args of [
    ["--app", fixtureApp, "--starting-backend-pid", "11"],
    ["--app", fixtureApp, "--state", state],
    ["--app", fixtureApp, "--starting-backend-pid", "1", "--state", state],
    ["--app", fixtureApp, "--starting-backend-pid", "11.5", "--state", state],
    [
      "--app",
      fixtureApp,
      "--starting-backend-pid",
      "11",
      "--state",
      "relative",
    ],
    ["--starting-backend-pid", "11", "--state", state],
  ])
    assert.throws(() => parseDeliveryArguments(args));
  assert.throws(
    () =>
      deliveryPlan({
        ...options,
        platform: "linux",
        commit: "b".repeat(40),
        repository,
      }),
    /Mac delivery/,
  );
});

test("explicit canary skip keeps target verification and the normal install owner", () => {
  const options = parseDeliveryArguments([
    "HEAD",
    "--app",
    fixtureApp,
    "--skip-canary",
  ]);
  const plan = deliveryPlan({
    ...options,
    platform: "darwin",
    commit: "b".repeat(40),
    repository,
  });
  assert.deepEqual(
    plan.map((step) => step.label),
    ["verify target", "record canary skip", "stage and verify"],
  );
  assert.equal(plan[0].args.at(-1), "--dry-run");
  assert.equal(plan[1].canarySkip.commit, "b".repeat(40));
  assert.equal(plan[1].canarySkip.app, fixtureApp);
  assert.ok(
    plan[2].args.some((argument) => argument.endsWith("deliver_server.py")),
  );
  assert.ok(!plan[2].args.includes("--dry-run"));
  assert.throws(() => parseDeliveryArguments(["--skip-canary"]), /--app/);
  assert.throws(
    () =>
      parseDeliveryArguments([
        "--app",
        fixtureApp,
        "--skip-canary",
        "--dry-run",
      ]),
    /--dry-run/,
  );
  assert.throws(
    () =>
      parseDeliveryArguments([
        "--app",
        fixtureApp,
        "--skip-canary",
        "--allow-retained-claude-steer-failure",
      ]),
    /combine/,
  );
  assert.throws(
    () =>
      deliveryPlan({
        ...options,
        platform: "linux",
        commit: "b".repeat(40),
        repository,
      }),
    /Mac delivery/,
  );
});

test("prepared skip delivery records the exact artifact without a passing canary verdict", (t) => {
  const folder = fs.mkdtempSync(
    path.join(os.tmpdir(), "delivery-canary-skip-"),
  );
  t.after(() => fs.rmSync(folder, { recursive: true, force: true }));
  const gate = {
    folder,
    requestId: "skip-one",
    commit: "b".repeat(40),
    app: fixtureApp,
    url: "http://127.0.0.1:4620",
  };
  const artifact = {
    build: { commit: gate.commit },
    bundleHash: "c".repeat(64),
    backendHash: "d".repeat(64),
  };
  assert.throws(
    () =>
      delivery.writeCanarySkip(
        gate,
        { ...artifact, build: { commit: "e".repeat(40) } },
        "f".repeat(40),
      ),
    /requested commit/,
  );
  const receiptFile = delivery.writeCanarySkip(gate, artifact, "f".repeat(40));
  const receipt = JSON.parse(fs.readFileSync(receiptFile, "utf8"));
  assert.equal(receipt.decision, "skipped");
  assert.equal(receipt.authorization, "--skip-canary");
  assert.equal(receipt.operatorCommit, "f".repeat(40));
  assert.deepEqual(receipt.request.artifact, artifact);
  assert.equal(receipt.report, undefined);
  assert.throws(
    () => delivery.writeCanarySkip(gate, artifact, "f".repeat(40)),
    /EEXIST/,
  );
  assert.equal(fs.statSync(receiptFile).mode & 0o777, 0o600);
});

test("only an explicit prepared-package skip may use a different operator commit", () => {
  const source = {
    commit: "b".repeat(40),
    head: "a".repeat(40),
    dirty: "",
    app: fixtureApp,
    skipCanary: true,
  };
  assert.doesNotThrow(() => delivery.validateDeliverySource(source));
  assert.throws(
    () => delivery.validateDeliverySource({ ...source, skipCanary: false }),
    /requested commit/,
  );
  assert.throws(
    () => delivery.validateDeliverySource({ ...source, app: undefined }),
    /requested commit/,
  );
  assert.throws(
    () => delivery.validateDeliverySource({ ...source, dirty: " M tracked" }),
    /tracked changes/,
  );
});

test("retained Claude steer admission is explicit and keeps the real canary", () => {
  const options = parseDeliveryArguments([
    "HEAD",
    "--app",
    fixtureApp,
    "--allow-retained-claude-steer-failure",
  ]);
  assert.equal(options.allowRetainedClaudeSteerFailure, true);
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
  assert.ok(plan[1].args.includes("--report-json"));
  assert.ok(plan[1].args.includes("--request-id"));
  assert.equal(plan[1].command, process.execPath);
  assert.equal(plan[1].canaryAdmission.allowRetainedClaudeSteerFailure, true);
});

test("only one retained Claude steer failure can be admitted without changing its verdict", (t) => {
  const expected = {
    requestId: "request-one",
    commit: "b".repeat(40),
    app: fixtureApp,
    build: { commit: "b".repeat(40) },
    backendHash: "c".repeat(64),
  };
  const report = {
    version: 1,
    requestId: expected.requestId,
    runId: "run-one",
    from: {
      app: "/Applications/Codex Studio.app",
      build: { commit: "89a7283a449fbbf9b86ae2d705b10fb8416cbb89" },
      backendHash:
        "e2987b057b2ebb4e2921c67e5a43112553b4ffb5a19fb31125cb8e87c970dcdd",
    },
    to: {
      app: fixtureApp,
      build: { commit: expected.commit },
      backendHash: expected.backendHash,
    },
    steer: {
      provider: "claude",
      accountKey: "claude-canary",
      chatId: "chat-one",
      messageId: "message-one",
      sourceCommit: "89a7283a449fbbf9b86ae2d705b10fb8416cbb89",
    },
    handles: {
      before: ["account:claude-canary|42|1"],
      after: ["account:claude-canary|42|1"],
    },
    rows: delivery.RETAINED_CLAUDE_CANARY_CHECKS?.map((name) => ({
      name,
      status: name === "steer delivered once" ? "FAIL" : "pass",
      detail:
        name === "steer delivered once"
          ? "answer CANARY_run-one_STEERED: not reached in 300 s (status running)"
          : "actual result",
      ms: 1,
    })),
  };
  assert.equal(
    delivery.canaryAdmission(report, expected, {
      exitCode: 1,
      allowRetainedClaudeSteerFailure: true,
    }),
    "retained-claude-steer-failure",
  );
  assert.equal(
    report.rows.find((row) => row.name === "steer delivered once").status,
    "FAIL",
  );
  for (const mutate of [
    (value) => value.rows.push({ name: "cleanup", status: "FAIL" }),
    (value) => value.rows.pop(),
    (value) => {
      value.rows[0].status = "skip";
    },
    (value) => {
      value.rows[0].status = "FAIL";
    },
    (value) => {
      value.steer.provider = "codex";
    },
    (value) => {
      value.from.build.commit = "d".repeat(40);
    },
    (value) => {
      value.from.backendHash = "d".repeat(64);
    },
    (value) => {
      value.to.backendHash = "d".repeat(64);
    },
    (value) => {
      value.to.build.commit = "d".repeat(40);
    },
    (value) => {
      value.requestId = "another-request";
    },
    (value) => {
      value.handles.after = [];
    },
    (value) => {
      value.rows.find((row) => row.name === "steer delivered once").detail =
        "message abc has 2 transcript copies";
    },
    (value) => {
      value.rows.find((row) => row.name === "steer delivered once").detail =
        "message abc receipt undefined";
    },
    (value) => {
      value.rows.find((row) => row.name === "steer delivered once").detail =
        "message abc receipt {materialized:false}";
    },
    (value) => {
      value.rows.find((row) => row.name === "steer delivered once").detail =
        "answer CANARY_other-run_STEERED: not reached in 300 s (status running)";
    },
    (value) => {
      value.rows.find((row) => row.name === "steer delivered once").detail =
        "answer CANARY_run-one_STEERED: not reached in 300 s (status paused)";
    },
    (value) => {
      value.rows.find((row) => row.name === "steer delivered once").detail =
        "answer CANARY_run-one_STEERED: not reached in 301 s (status running)";
    },
    (value) => {
      value.rows.find((row) => row.name === "steer delivered once").detail =
        "chat abc is failed";
    },
  ]) {
    const value = structuredClone(report);
    mutate(value);
    assert.throws(() =>
      delivery.canaryAdmission(value, expected, {
        exitCode: 1,
        allowRetainedClaudeSteerFailure: true,
      }),
    );
  }
  assert.throws(() =>
    delivery.canaryAdmission(report, expected, { exitCode: 1 }),
  );
  assert.throws(() =>
    delivery.canaryAdmission(report, expected, {
      exitCode: 2,
      allowRetainedClaudeSteerFailure: true,
    }),
  );
  const folder = fs.mkdtempSync(
    path.join(os.tmpdir(), "delivery-canary-admission-"),
  );
  t.after(() => fs.rmSync(folder, { recursive: true, force: true }));
  const artifact = {
    build: expected.build,
    backendHash: expected.backendHash,
    bundleHash: "f".repeat(64),
  };
  const gate = {
    ...expected,
    folder,
    url: "http://127.0.0.1:4620",
    allowRetainedClaudeSteerFailure: true,
  };
  const saved = delivery.writeCanaryAdmission(gate, artifact, report, 1);
  const receipt = JSON.parse(fs.readFileSync(saved.receiptFile, "utf8"));
  assert.equal(
    receipt.report.rows.find((row) => row.name === "steer delivered once")
      .status,
    "FAIL",
  );
  assert.deepEqual(receipt.request.artifact, artifact);
  assert.equal(receipt.request.requestId, expected.requestId);
  assert.equal(receipt.canaryExitCode, 1);
  assert.throws(
    () => delivery.writeCanaryAdmission(gate, artifact, report, 1),
    /EEXIST/,
  );
  const passing = structuredClone(report);
  passing.rows.forEach((row) => {
    row.status = "pass";
  });
  assert.equal(
    delivery.canaryAdmission(passing, expected, { exitCode: 0 }),
    "pass",
  );
  assert.throws(() =>
    delivery.canaryAdmission(passing, expected, {
      exitCode: 1,
      allowRetainedClaudeSteerFailure: true,
    }),
  );
});

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
  const options = parseDeliveryArguments(["HEAD", "--app", fixtureApp]);
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
    fixtureApp,
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

test("startup delivery keeps process proof, the legacy swap, and the existing recovery owner", async () => {
  execFileSync(
    await deliveryPython(),
    ["-I", "-B", "workspaces/client/apps/desktop/startup-delivery-test.py"],
    {
      cwd: repository,
      encoding: "utf8",
      stdio: ["ignore", "pipe", "pipe"],
    },
  );
});

test("the Mac delivery entry point works under Python isolated mode", async () => {
  const output = execFileSync(
    await deliveryPython(),
    ["-I", "-B", "workspaces/client/apps/desktop/deliver_server.py", "--help"],
    { cwd: repository, encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] },
  );
  assert.ok(output.includes("--commit"));
});
