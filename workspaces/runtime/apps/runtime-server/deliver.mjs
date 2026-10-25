// Platform router for the single operator delivery command.
import { execFileSync } from "node:child_process";
import { randomUUID } from "node:crypto";
import fs from "node:fs";
import os from "node:os";
import { createRequire } from "node:module";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { RETAINED_CLAUDE_CANARY_CHECKS } from "../../../tooling/apps/upgrade-canary/canary.mjs";
export { RETAINED_CLAUDE_CANARY_CHECKS };

const RETAINED_CLAUDE_SOURCE_COMMIT =
  "89a7283a449fbbf9b86ae2d705b10fb8416cbb89";
const RETAINED_CLAUDE_SOURCE_HASH =
  "e2987b057b2ebb4e2921c67e5a43112553b4ffb5a19fb31125cb8e87c970dcdd";

export function canaryAdmission(
  report,
  expected,
  { exitCode, allowRetainedClaudeSteerFailure = false },
) {
  const refuse = () => {
    throw new Error("The canary report does not permit this delivery.");
  };
  if (
    ![0, 1].includes(exitCode) ||
    report?.version !== 1 ||
    report.requestId !== expected.requestId ||
    !report.runId ||
    report.to?.app !== expected.app ||
    report.to?.build?.commit !== expected.commit ||
    JSON.stringify(report.to?.build) !== JSON.stringify(expected.build) ||
    report.to?.backendHash !== expected.backendHash ||
    !Array.isArray(report.rows) ||
    report.rows.length !== RETAINED_CLAUDE_CANARY_CHECKS.length ||
    report.rows.some(
      (row, index) =>
        row.name !== RETAINED_CLAUDE_CANARY_CHECKS[index] ||
        !["pass", "FAIL"].includes(row.status),
    )
  )
    refuse();
  const failed = report.rows.filter((row) => row.status === "FAIL");
  if (!failed.length && exitCode === 0) return "pass";
  const steer = report.steer;
  const before = report.handles?.before;
  const after = report.handles?.after;
  if (
    !allowRetainedClaudeSteerFailure ||
    exitCode !== 1 ||
    failed.length !== 1 ||
    failed[0].name !== "steer delivered once" ||
    failed[0].detail !==
      `answer CANARY_${report.runId}_STEERED: not reached in 300 s (status running)` ||
    report.from?.build?.commit !== RETAINED_CLAUDE_SOURCE_COMMIT ||
    report.from?.backendHash !== RETAINED_CLAUDE_SOURCE_HASH ||
    steer?.provider !== "claude" ||
    steer.accountKey !== "claude-canary" ||
    !steer.chatId ||
    !steer.messageId ||
    steer.sourceCommit !== RETAINED_CLAUDE_SOURCE_COMMIT ||
    !Array.isArray(before) ||
    !before.length ||
    !before.some((line) =>
      /^account:claude-canary\|[1-9][0-9]*\|[1-9][0-9]*$/.test(line),
    ) ||
    JSON.stringify(before) !== JSON.stringify(after)
  )
    refuse();
  return "retained-claude-steer-failure";
}

export function writeCanaryAdmission(gate, artifact, report, exitCode) {
  const expected = {
    requestId: gate.requestId,
    commit: gate.commit,
    app: gate.app,
    build: artifact.build,
    backendHash: artifact.backendHash,
  };
  const decision = canaryAdmission(report, expected, {
    exitCode,
    allowRetainedClaudeSteerFailure:
      gate.allowRetainedClaudeSteerFailure === true,
  });
  const receipt = {
    version: 1,
    request: { ...expected, url: gate.url, artifact },
    decision,
    authorization: "--allow-retained-claude-steer-failure",
    canaryExitCode: exitCode,
    report,
    recordedAt: new Date().toISOString(),
  };
  const receiptFile = path.join(
    gate.folder,
    `canary-admission-${gate.requestId}.json`,
  );
  fs.writeFileSync(receiptFile, JSON.stringify(receipt, null, 2) + "\n", {
    mode: 0o600,
    flag: "wx",
  });
  return { decision, receiptFile };
}

export function writeCanarySkip(gate, artifact, operatorCommit) {
  if (artifact.build?.commit !== gate.commit)
    throw new Error("The candidate differs from the requested commit.");
  const receiptFile = path.join(
    gate.folder,
    `canary-skip-${gate.requestId}.json`,
  );
  fs.writeFileSync(
    receiptFile,
    JSON.stringify(
      {
        version: 1,
        request: {
          requestId: gate.requestId,
          commit: gate.commit,
          app: gate.app,
          url: gate.url,
          artifact,
        },
        decision: "skipped",
        authorization: "--skip-canary",
        operatorCommit,
        recordedAt: new Date().toISOString(),
      },
      null,
      2,
    ) + "\n",
    { mode: 0o600, flag: "wx" },
  );
  return receiptFile;
}

export function validateDeliverySource({
  commit,
  head,
  dirty,
  app,
  skipCanary,
}) {
  if (commit !== head && !(app && skipCanary))
    throw new Error(
      "Use a worktree at the requested commit before Mac delivery.",
    );
  if (dirty) throw new Error("Commit the tracked changes before Mac delivery.");
}

// Reuse the staging owner's complete bundle hash, not a second hash definition.
export function deliveryArtifact(app, python) {
  return JSON.parse(
    execFileSync(
      python,
      [
        "-I",
        "-B",
        "-c",
        "import json,runpy,sys; from pathlib import Path; m=runpy.run_path(sys.argv[1]); p=Path(sys.argv[2]); print(json.dumps({'build':m['updates'].read_build(p),'bundleHash':m['bundle_hash'](p),'backendHash':m['file_hash'](m['server_path'](p))}))",
        path.join(
          repository,
          "workspaces/client/apps/desktop/deliver_server.py",
        ),
        app,
      ],
      { encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] },
    ),
  );
}

const here = path.dirname(fileURLToPath(import.meta.url));
const repository = path.resolve(here, "../../../..");
const require = createRequire(import.meta.url);
const {
  supportedPython,
} = require("../../../client/apps/desktop/recovery.cjs");

export function deliveryPython(env = process.env) {
  return supportedPython({
    env,
    saved: {},
    script: path.join(
      repository,
      "workspaces/client/apps/desktop/recover_backend.py",
    ),
  });
}

export function parseDeliveryArguments(argv) {
  const options = {
    ref: "HEAD",
    app: undefined,
    dryRun: false,
    url: undefined,
    receipts: undefined,
    allowRetainedClaudeSteerFailure: false,
    skipCanary: false,
  };
  const args = [...argv];
  if (args[0] && !args[0].startsWith("--")) options.ref = args.shift();
  while (args.length) {
    const flag = args.shift();
    if (flag === "--dry-run") options.dryRun = true;
    else if (flag === "--skip-canary") options.skipCanary = true;
    else if (flag === "--allow-retained-claude-steer-failure")
      options.allowRetainedClaudeSteerFailure = true;
    else if (["--app", "--url", "--receipts"].includes(flag)) {
      const value = args.shift();
      if (!value || value.startsWith("--"))
        throw new Error(`Missing value for ${flag}`);
      options[flag.slice(2)] = value;
    } else throw new Error(`Unknown delivery option: ${flag}`);
  }
  if (options.dryRun && !options.app)
    throw new Error(
      "Provide --app for a dry run; it must not build a package.",
    );
  if (options.dryRun && options.allowRetainedClaudeSteerFailure)
    throw new Error(
      "The retained Claude steer admission requires a real canary, not --dry-run.",
    );
  if (options.skipCanary) {
    if (!options.app) throw new Error("Provide --app with --skip-canary.");
    if (options.dryRun)
      throw new Error("Do not combine --skip-canary with --dry-run.");
    if (options.allowRetainedClaudeSteerFailure)
      throw new Error(
        "Do not combine --skip-canary with retained Claude admission.",
      );
  }
  return options;
}

export function deliveryPlan({
  platform,
  commit,
  app,
  dryRun = false,
  url,
  receipts,
  allowRetainedClaudeSteerFailure = false,
  skipCanary = false,
  repository: root = repository,
}) {
  if (platform === "linux") {
    if (
      app ||
      dryRun ||
      url ||
      receipts ||
      allowRetainedClaudeSteerFailure ||
      skipCanary
    )
      throw new Error("Mac delivery options are not supported on Linux");
    return [
      {
        label: "Linux delivery",
        command: "bash",
        args: [
          path.join(root, "workspaces/runtime/apps/runtime-server/deliver.sh"),
          commit,
        ],
      },
    ];
  }
  if (platform !== "darwin")
    throw new Error(`Unsupported delivery platform: ${platform}`);
  const folder = path.resolve(
    receipts ??
      path.join(os.homedir(), ".cache/codex-studio/deliveries", commit),
  );
  const contract = JSON.parse(
    fs.readFileSync(
      path.join(root, "workspaces/client/apps/desktop/delivery-contract.json"),
      "utf8",
    ),
  );
  const native = JSON.parse(
    fs.readFileSync(
      path.join(
        root,
        "workspaces/client/apps/native/src-tauri/tauri.conf.json",
      ),
      "utf8",
    ),
  );
  const application = path.resolve(
    app ?? path.join(folder, "package", `${native.productName}.app`),
  );
  const layout = JSON.parse(
    fs.readFileSync(
      path.join(
        root,
        "workspaces/runtime/apps/runtime-server/service-layout.json",
      ),
      "utf8",
    ),
  );
  const steps = [];
  if (!app && !fs.existsSync(application))
    steps.push({
      label: "package",
      command: "just",
      args: ["package-native", path.join(folder, "package")],
    });
  const stage = {
    label: "stage and verify",
    command: "python3",
    args: [
      "-I",
      "-B",
      path.join(root, "workspaces/client/apps/desktop/deliver_server.py"),
      "--app",
      application,
      "--commit",
      commit,
      "--receipts",
      folder,
      "--url",
      url ?? `http://127.0.0.1:${layout.port}`,
    ],
  };
  if (dryRun)
    return [{ ...stage, label: "dry run", args: [...stage.args, "--dry-run"] }];
  let saved;
  try {
    saved = JSON.parse(
      fs.readFileSync(path.join(folder, contract.receiptFile), "utf8"),
    );
  } catch {
    /* The staging owner reports unreadable receipts before a live write. */
  }
  const resume =
    saved?.request?.commit === commit &&
    Object.values(contract.phases).includes(saved.phase);
  if (!resume || allowRetainedClaudeSteerFailure || skipCanary) {
    steps.push({
      ...stage,
      label: "verify target",
      args: [...stage.args, "--dry-run"],
    });
    if (skipCanary) {
      steps.push({
        label: "record canary skip",
        canarySkip: {
          requestId: randomUUID(),
          folder,
          app: application,
          commit,
          url: url ?? `http://127.0.0.1:${layout.port}`,
        },
      });
    } else {
      const requestId = allowRetainedClaudeSteerFailure
        ? randomUUID()
        : undefined;
      const reportFile =
        requestId && path.join(folder, `canary-${requestId}.json`);
      steps.push({
        label: "upgrade canary",
        command: reportFile ? process.execPath : "just",
        args: [
          reportFile
            ? path.join(
                root,
                "workspaces/tooling/apps/upgrade-canary/canary.mjs",
              )
            : "upgrade-canary",
          "--to",
          application,
          ...(reportFile
            ? ["--report-json", reportFile, "--request-id", requestId]
            : []),
        ],
        ...(reportFile
          ? {
              canaryAdmission: {
                requestId,
                reportFile,
                folder,
                app: application,
                commit,
                url: url ?? `http://127.0.0.1:${layout.port}`,
                allowRetainedClaudeSteerFailure,
              },
            }
          : {}),
      });
    }
  }
  steps.push(stage);
  return steps;
}

async function main(argv) {
  const options = parseDeliveryArguments(argv);
  const git = (args) =>
    execFileSync("git", args, {
      cwd: repository,
      encoding: "utf8",
      stdio: ["ignore", "pipe", "pipe"],
    }).trim();
  const commit = git(["rev-parse", "--verify", `${options.ref}^{commit}`]);
  const operatorCommit = git(["rev-parse", "HEAD"]);
  if (process.platform === "darwin") {
    validateDeliverySource({
      ...options,
      commit,
      head: operatorCommit,
      dirty: git(["status", "--porcelain", "--untracked-files=no"]),
    });
  }
  const python =
    process.platform === "darwin" ? await deliveryPython() : undefined;
  let skippedArtifact;
  for (const step of deliveryPlan({
    ...options,
    platform: process.platform,
    commit,
  })) {
    console.log(step.label);
    if (step.canarySkip) {
      const gate = step.canarySkip;
      skippedArtifact = deliveryArtifact(gate.app, python);
      fs.mkdirSync(gate.folder, { recursive: true, mode: 0o700 });
      const receiptFile = writeCanarySkip(
        gate,
        skippedArtifact,
        operatorCommit,
      );
      console.log(
        `Canary skipped by explicit request; recorded in ${receiptFile}`,
      );
      continue;
    }
    if (step.canaryAdmission) {
      const gate = step.canaryAdmission;
      const artifact = deliveryArtifact(gate.app, python);
      if (artifact.build?.commit !== commit)
        throw new Error(
          "The canary candidate differs from the requested commit.",
        );
      fs.mkdirSync(gate.folder, { recursive: true, mode: 0o700 });
      let exitCode = 0;
      try {
        execFileSync(step.command, step.args, {
          cwd: repository,
          stdio: "inherit",
          env: process.env,
        });
      } catch (error) {
        if (error.status !== 1 || error.signal) throw error;
        exitCode = error.status;
      }
      const report = JSON.parse(fs.readFileSync(gate.reportFile, "utf8"));
      if (
        JSON.stringify(deliveryArtifact(gate.app, python)) !==
        JSON.stringify(artifact)
      )
        throw new Error("The candidate artifact changed during the canary.");
      const { decision, receiptFile } = writeCanaryAdmission(
        gate,
        artifact,
        report,
        exitCode,
      );
      console.log(
        `Canary admission: ${decision}; original verdict preserved in ${receiptFile}`,
      );
      continue;
    }
    if (
      skippedArtifact &&
      JSON.stringify(deliveryArtifact(options.app, python)) !==
        JSON.stringify(skippedArtifact)
    )
      throw new Error("The candidate artifact changed after the canary skip.");
    execFileSync(
      step.command === "python3" ? python : step.command,
      step.args,
      {
        cwd: repository,
        stdio: "inherit",
        env: process.env,
      },
    );
  }
}

if (
  process.argv[1] &&
  import.meta.url === pathToFileURL(path.resolve(process.argv[1])).href
) {
  main(process.argv.slice(2)).catch((error) => {
    console.error(error.message);
    process.exitCode = 1;
  });
}
