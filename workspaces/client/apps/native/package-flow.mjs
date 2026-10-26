// The native package cycle in one command: `just package-native <out> [flags]`.
//
//   1. dev-profile native host check (skip: --hosted-ci-passed)
//   2. Rust release binaries for HEAD, reused when the manifest verifies
//   3. package.mjs (signs with the configured identity)
//   4. check-package.mjs and check-package-start.py
//
// --hosted-ci-passed states that hosted CI already passed on the exact HEAD commit,
// so the dev-profile build of the native host (cargo build --features native-check)
// and its hidden WebView scenarios are not repeated here. Without the flag the
// check runs, as before.
import { execFileSync } from "node:child_process";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import {
  buildReleaseBinaries,
  checkoutState,
  releaseTargetDirectory,
  releaseTargetEnvironment,
  verifyReleaseBinaries,
} from "../desktop/release-binaries.mjs";

const root = path.dirname(fileURLToPath(import.meta.url));
const repository = path.resolve(root, "../../../..");

export function parseFlowArguments(argv) {
  const flags = new Set(["--hosted-ci-passed"]);
  const positional = [];
  let hostedCiPassed = false;
  for (const argument of argv) {
    if (!argument.startsWith("--")) positional.push(argument);
    else if (flags.has(argument)) hostedCiPassed = true;
    else throw new Error(`Unknown flag: ${argument}`);
  }
  if (positional.length !== 1)
    throw new Error(
      "Usage: package-flow.mjs <new output directory> [--hosted-ci-passed]",
    );
  return { out: path.resolve(positional[0]), hostedCiPassed };
}

// Ordered steps of the flow. Each step is data, so a test can check the plan
// without running anything.
export function planFlow({ out, hostedCiPassed, directory, commit }) {
  const application = path.join(out, "Codex Studio.app");
  const environment = {
    CODEX_NATIVE_PACKAGE_OUT: out,
    [releaseTargetEnvironment]: directory,
  };
  return [
    hostedCiPassed
      ? {
          label: "native host check skipped",
          skipped: `hosted CI passed on ${commit} (--hosted-ci-passed)`,
        }
      : {
          label: "native host check",
          command: "just",
          args: ["test-native-webview"],
          // check-native.py reads target/debug; keep the dev build out of the
          // shared release directory.
          withoutEnvironment: ["CARGO_TARGET_DIR", releaseTargetEnvironment],
        },
    { label: "release binaries", releaseBinaries: true },
    {
      label: "package",
      command: "pnpm",
      args: ["--filter", "studio-native-client", "run", "package"],
      environment,
    },
    {
      label: "check package",
      command: "node",
      args: [path.join(root, "check-package.mjs"), application],
    },
    {
      label: "check package start",
      command: "python3",
      args: [path.join(root, "check-package-start.py"), application],
    },
  ];
}

async function ensureReleaseBinaries(directory, commit) {
  try {
    await verifyReleaseBinaries({ directory, commit });
    console.log(`Reusing release binaries for ${commit} in ${directory}`);
  } catch (error) {
    console.log(`Building release binaries: ${error.message}`);
    await buildReleaseBinaries({ repository, directory });
  }
}

async function main(argv) {
  const { out, hostedCiPassed } = parseFlowArguments(argv);
  const { commit, dirty } = checkoutState(repository);
  if (dirty)
    throw new Error(
      "Tracked files differ from HEAD. Commit them first: the package, the manifest and the CI claim name one commit.",
    );
  const directory = releaseTargetDirectory();
  for (const step of planFlow({ out, hostedCiPassed, directory, commit })) {
    console.log(`== ${step.label}${step.skipped ? `: ${step.skipped}` : ""}`);
    if (step.skipped) continue;
    if (step.releaseBinaries) {
      await ensureReleaseBinaries(directory, commit);
      continue;
    }
    const env = { ...process.env, ...step.environment };
    for (const name of step.withoutEnvironment ?? []) delete env[name];
    execFileSync(step.command, step.args, {
      cwd: repository,
      stdio: "inherit",
      env,
    });
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href)
  main(process.argv.slice(2)).catch((error) => {
    console.error(error.message);
    process.exitCode = 1;
  });
