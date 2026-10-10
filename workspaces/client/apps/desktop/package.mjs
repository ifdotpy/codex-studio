import { buildLinuxVM } from "./native/linux-vm/build.mjs";
import { signingIdentity, signApplication, signCode } from "./signing.mjs";
import { execFileSync } from "node:child_process";
import { packager } from "@electron/packager";
import { cp, mkdir, mkdtemp, rm, access, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
const root = path.dirname(fileURLToPath(import.meta.url));
const repositoryRoot = path.resolve(root, "../../../..");
const packageOutput = path.resolve(
  process.env.CODEX_DESKTOP_PACKAGE_OUT || path.join(root, "dist"),
);
const uiOnly =
  process.argv.includes("--ui-only") ||
  process.env.CODEX_DESKTOP_UI_ONLY === "1";
const identity = signingIdentity();
const rendererDist = path.join(
  repositoryRoot,
  "workspaces/client/apps/web/dist",
);
const backendSource = path.join(
  repositoryRoot,
  "workspaces/runtime/apps/server/src",
);
const bridgeSource = path.join(
  repositoryRoot,
  "workspaces/providers/apps/claude-bridge",
);
await access(path.join(rendererDist, "index.html"));
if (!uiOnly) {
  try {
    execFileSync(
      "cargo",
      [
        "build",
        "--locked",
        "--release",
        "-p",
        "studio-diagnostics",
        "-p",
        "studio-operations-python",
      ],
      { cwd: repositoryRoot, stdio: "inherit" },
    );
  } catch (error) {
    if (error.code === "ENOENT") {
      throw new Error(
        "Desktop packaging requires Cargo; install Rust 1.99.0, then run `cargo build --release -p studio-operations-python`.",
        { cause: error },
      );
    }
    throw error;
  }
}
const cargoTargetDirectory = path.resolve(
  repositoryRoot,
  process.env.CARGO_TARGET_DIR || "target",
);
const diagnosticsBinary = path.join(
  cargoTargetDirectory,
  "release/codex-diagnostics",
);
const operationsLibrary = path.join(
  cargoTargetDirectory,
  "release",
  process.platform === "darwin"
    ? "libstudio_operations_native.dylib"
    : process.platform === "win32"
      ? "studio_operations_native.dll"
      : "libstudio_operations_native.so",
);
const operationsModuleName =
  process.platform === "win32"
    ? "studio_operations_native.pyd"
    : "studio_operations_native.abi3.so";
const stage = await mkdtemp(path.join(tmpdir(), "codex-desktop-package-"));
try {
  const speech = path.join(stage, "studio-speech");
  execFileSync("xcrun", [
    "swiftc",
    path.join(root, "native/speech.swift"),
    "-O",
    "-o",
    speech,
    "-Xlinker",
    "-sectcreate",
    "-Xlinker",
    "__TEXT",
    "-Xlinker",
    "__info_plist",
    "-Xlinker",
    path.join(root, "native/speech-info.plist"),
  ]);
  signCode(speech, identity);
  const linuxVM = uiOnly
    ? undefined
    : buildLinuxVM(path.join(stage, "studio-linux-vm"), identity);
  const resources = path.join(stage, "workspace");
  await mkdir(path.join(resources, "web"), { recursive: true });
  if (!uiOnly) {
    await cp(
      path.join(repositoryRoot, "requirements.txt"),
      path.join(resources, "requirements.txt"),
    );
    await mkdir(path.join(resources, "bin"), { recursive: true });
    await cp(
      operationsLibrary,
      path.join(resources, "bin", operationsModuleName),
    );
    await cp(
      path.join(repositoryRoot, "workspaces/runtime/apps/server/prompts"),
      path.join(resources, "prompts"),
      { recursive: true },
    );
    await cp(backendSource, path.join(resources, "scripts"), {
      recursive: true,
      filter: (source) =>
        path.basename(source) !== ".studio-update.lock" &&
        !source.includes("__pycache__") &&
        !source.includes("/node_modules") &&
        !source.endsWith(".pyc"),
    });
    await cp(diagnosticsBinary, path.join(resources, "bin/codex-diagnostics"));
    execFileSync(
      "pnpm",
      [
        "--filter",
        "studio-claude-bridge",
        "--prod",
        "deploy",
        "--no-optional",
        "--ignore-scripts",
        "--node-linker=hoisted",
        path.join(resources, "scripts/claude_bridge"),
      ],
      {
        cwd: repositoryRoot,
        stdio: "inherit",
      },
    );
    await rm(path.join(resources, "scripts/claude_bridge/node_modules/.bin"), {
      recursive: true,
      force: true,
    });
    await cp(
      bridgeSource,
      path.join(resources, "workspaces/providers/apps/claude-bridge"),
      {
        recursive: true,
        filter: (source) =>
          path.basename(source) !== "node_modules" &&
          !source.includes("/node_modules/") &&
          path.basename(source) !== "dist" &&
          path.basename(source) !== "coverage",
      },
    );
    for (const [source, destination] of [
      ["package.json", "package.json"],
      ["pnpm-workspace.yaml", "pnpm-workspace.yaml"],
      ["pnpm-lock.yaml", "pnpm-lock.yaml"],
      ["patches/rxdb@17.5.0.patch", "patches/rxdb@17.5.0.patch"],
    ]) {
      const target = path.join(resources, destination);
      await mkdir(path.dirname(target), { recursive: true });
      await cp(path.join(repositoryRoot, source), target);
    }
    await cp(
      path.join(repositoryRoot, "workspaces/runtime/apps/vm-guest"),
      path.join(resources, "vm/guest"),
      {
        recursive: true,
        filter: (source) =>
          !source.includes("__pycache__") && !source.endsWith(".pyc"),
      },
    );
  }
  await cp(rendererDist, path.join(resources, "web/dist"), {
    recursive: true,
  });
  if (!uiOnly) {
    await cp(
      path.join(repositoryRoot, ".agents/skills"),
      path.join(resources, ".agents/skills"),
      { recursive: true },
    );
    for (const [source, destination] of [
      ["README.md", "README.md"],
      ["docs/cli.md", "CLI.md"],
      ["docs/orchestration.md", "ORCHESTRATION.md"],
    ]) {
      await cp(
        path.join(repositoryRoot, source),
        path.join(resources, destination),
      );
    }
  }
  await writeFile(
    path.join(resources, "studio-install.json"),
    JSON.stringify({ mode: uiOnly ? "ui-only" : "combined" }),
  );
  const output = await packager({
    dir: root,
    name: "Codex Studio",
    executableName: "Codex Studio",
    icon: path.join(root, "assets/codex-studio.icns"),
    appBundleId: "local.codex.agents",
    appCategoryType: "public.app-category.developer-tools",
    platform: "darwin",
    arch: "arm64",
    electronVersion: "44.2.0",
    out: packageOutput,
    overwrite: true,
    asar: true,
    prune: true,
    extraResource: [
      resources,
      speech,
      ...(uiOnly ? [] : [linuxVM, path.join(root, "recover_backend.py")]),
    ],
    extendInfo: {
      NSMicrophoneUsageDescription:
        "Record dictation that you can review and convert to text.",
      NSSpeechRecognitionUsageDescription:
        "Convert your saved dictation to text on this computer.",
    },
    ignore: [
      /^\/assets($|\/)/,
      /^\/dist($|\/)/,
      /^\/node_modules($|\/)/,
      /^\/test\.mjs$/,
      /^\/multi-server-memory\.mjs$/,
      /^\/(?:backend-test|recovery-test|recovery-renderer-test|renderer-recovery-test|window-state-test|package-test)\.(?:mjs|cjs|py)$/,
      /^\/recover_backend\.py$/,
      /^\/__pycache__($|\/)/,
      /^\/package\.mjs$/,
      /^\/README\.md$/,
    ],
  });
  for (const directory of output) {
    const application = path.join(directory, "Codex Studio.app");
    signApplication(application, identity);
  }
  console.log(output.join("\n"));
} finally {
  await rm(stage, { recursive: true, force: true });
}
