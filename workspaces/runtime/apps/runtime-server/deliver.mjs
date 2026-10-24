// Platform router for the single operator delivery command.
import { execFileSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import { createRequire } from "node:module";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

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
  };
  const args = [...argv];
  if (args[0] && !args[0].startsWith("--")) options.ref = args.shift();
  while (args.length) {
    const flag = args.shift();
    if (flag === "--dry-run") options.dryRun = true;
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
  return options;
}

export function deliveryPlan({
  platform,
  commit,
  app,
  dryRun = false,
  url,
  receipts,
  repository: root = repository,
}) {
  if (platform === "linux") {
    if (app || dryRun || url || receipts)
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
  if (!resume) {
    steps.push({
      ...stage,
      label: "verify target",
      args: [...stage.args, "--dry-run"],
    });
    steps.push({
      label: "upgrade canary",
      command: "just",
      args: ["upgrade-canary", "--to", application],
    });
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
  if (process.platform === "darwin") {
    if (commit !== git(["rev-parse", "HEAD"]))
      throw new Error(
        "Use a worktree at the requested commit before Mac delivery.",
      );
    if (git(["status", "--porcelain", "--untracked-files=no"]))
      throw new Error("Commit the tracked changes before Mac delivery.");
  }
  const python =
    process.platform === "darwin" ? await deliveryPython() : undefined;
  for (const step of deliveryPlan({
    ...options,
    platform: process.platform,
    commit,
  })) {
    console.log(step.label);
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
