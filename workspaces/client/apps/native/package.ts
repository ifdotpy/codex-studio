import { execFileSync } from "node:child_process";
import { cp, mkdir, mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { signingIdentity, signApplication } from "../desktop/signing.mjs";
import { setBundleDisplayName } from "../desktop/bundle-name.mjs";
import {
  prebuiltReleaseDirectory,
  releaseTargetEnvironment,
} from "../desktop/release-binaries.mjs";
import { packageRecoveryHelpers } from "./recovery-package.ts";
import { pruneBridgeImageCodecs } from "./bridge-image-package.ts";

const root = path.dirname(fileURLToPath(import.meta.url));
const repository = path.resolve(root, "../../../..");
if (process.platform !== "darwin")
  throw new Error("Packaging currently requires macOS.");
if (!process.env.CODEX_NATIVE_PACKAGE_OUT)
  throw new Error("Set CODEX_NATIVE_PACKAGE_OUT to a new artifact directory.");
const output = path.resolve(process.env.CODEX_NATIVE_PACKAGE_OUT);
// With CODEX_RELEASE_TARGET_DIR set, the Rust release binaries must come from that
// directory and match this exact commit; a mismatch stops here, before any build.
// The Tauri client builds in the same directory and reuses its compiled crates.
const prebuiltRelease = await prebuiltReleaseDirectory(repository);
const cargoEnvironment = prebuiltRelease
  ? {
      ...process.env,
      [releaseTargetEnvironment]: prebuiltRelease,
      CARGO_TARGET_DIR: prebuiltRelease,
    }
  : process.env;
const identity = signingIdentity();
// Refuse to overwrite a previously verified artifact.
await mkdir(output, { recursive: false });
const stage = await mkdtemp(path.join(tmpdir(), "studio-native-package-"));
try {
  execFileSync(
    "pnpm",
    ["exec", "tauri", "build", "--ci", "--bundles", "app", "--", "--locked"],
    {
      cwd: root,
      stdio: "inherit",
      env: cargoEnvironment,
    },
  );
  execFileSync(
    process.execPath,
    [path.join(root, "../desktop/package.mjs"), "--resources-only"],
    {
      cwd: repository,
      stdio: "inherit",
      env: {
        ...cargoEnvironment,
        CODEX_DESKTOP_PACKAGE_OUT: stage,
        CODEX_DESKTOP_UI_ONLY: "0",
      },
    },
  );
  await pruneBridgeImageCodecs(
    path.join(stage, "workspace/scripts/claude_bridge"),
  );
  const target =
    prebuiltRelease ??
    path.resolve(repository, process.env.CARGO_TARGET_DIR || "target");
  const application = path.join(output, "Codex Studio.app");
  await cp(
    path.join(target, "release/bundle/macos/Codex Studio.app"),
    application,
    { recursive: true },
  );
  const resources = path.join(application, "Contents/Resources");
  await cp(stage, resources, { recursive: true });
  await mkdir(path.join(resources, "helpers"), { recursive: true });
  await cp(
    path.join(resources, "studio-speech"),
    path.join(resources, "helpers/studio-speech"),
  );
  await packageRecoveryHelpers(resources);
  setBundleDisplayName(application, "LAYR Studio");
  signApplication(application, identity);
  console.log(application);
} finally {
  await rm(stage, { recursive: true, force: true });
}
