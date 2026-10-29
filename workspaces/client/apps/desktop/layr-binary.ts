import { execFileSync } from "node:child_process";
import type { ExecFileSyncOptions } from "node:child_process";
import { access, cp, mkdir, readFile, writeFile } from "node:fs/promises";
import { constants } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { sha256File } from "./release-binaries.ts";

const repositoryRoot = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../../../..",
);

// The runtime owns the required version and manifest name. Packaging consumes
// those constants directly so a version change cannot leave a second contract.
export async function layrBundleContract(repository = repositoryRoot) {
  const source = path.join(
    repository,
    "workspaces/runtime/packages/runtime-services/src",
  );
  const [native, installer] = await Promise.all([
    readFile(path.join(source, "layr_native.rs"), "utf8"),
    readFile(path.join(source, "layr_native/install.rs"), "utf8"),
  ]);
  const version = native.match(/\bconst LAYR_VERSION: &str = "([^"]+)";/)?.[1];
  const manifestName = installer.match(
    /\bconst SHIPPED_MANIFEST_NAME: &str = "([^"]+)";/,
  )?.[1];
  const versionTimeout = installer.match(
    /\bconst VERSION_CHECK_TIMEOUT: Duration = Duration::from_secs\((\d+)\);/,
  )?.[1];
  if (!version || !manifestName || !versionTimeout)
    throw new Error("Cannot read the native Layr bundle contract.");
  return {
    version,
    manifestName,
    versionTimeoutMs: Number(versionTimeout) * 1000,
  };
}

// Always run Cargo so it checks the source inputs. Its persistent target folder
// reuses release artifacts across package builds, including resources-only builds.
export async function bundleLayrBinary({
  repository = repositoryRoot,
  workspace,
  targetDirectory,
  sign,
  run = (command, args, options) => execFileSync(command, args, options),
}: {
  repository?: string;
  workspace: string;
  targetDirectory: string;
  sign: (binary: string) => unknown;
  run?: (
    command: string,
    args: string[],
    options: ExecFileSyncOptions,
  ) => unknown;
}) {
  const { version, manifestName } = await layrBundleContract(repository);
  const manifest = path.join(
    repository,
    "workspaces/runtime/apps/layr/Cargo.toml",
  );
  const cargo = await readFile(manifest, "utf8");
  const sourceVersion = cargo.match(
    /\[package\][\s\S]*?^version\s*=\s*"([^"]+)"/m,
  )?.[1];
  if (sourceVersion !== version)
    throw new Error(`The host source must contain Layr ${version}.`);
  const target = targetDirectory;
  const sourceManifest: unknown = JSON.parse(
    await readFile(path.join(path.dirname(manifest), "manifest.json"), "utf8"),
  );
  if (
    !sourceManifest ||
    typeof sourceManifest !== "object" ||
    !("revision" in sourceManifest) ||
    typeof sourceManifest.revision !== "string" ||
    !/^[a-f0-9]{40}$/.test(sourceManifest.revision)
  )
    throw new Error(
      "The Layr source manifest must name a full upstream revision.",
    );
  run(
    "cargo",
    [
      "build",
      "--release",
      "--locked",
      "-j",
      "4",
      "--manifest-path",
      manifest,
      "--target-dir",
      target,
    ],
    {
      cwd: repository,
      stdio: "inherit",
      env: {
        ...process.env,
        CARGO_INCREMENTAL: "0",
        LAYR_BUILD_ID: sourceManifest.revision,
      },
    },
  );
  const binary = path.join(workspace, "bin/layr");
  await mkdir(path.dirname(binary), { recursive: true });
  await cp(path.join(target, "release/layr"), binary);
  await sign(binary);
  await writeFile(
    path.join(workspace, "bin", manifestName),
    `${JSON.stringify({ version, sha256: await sha256File(binary) }, null, 2)}\n`,
  );
  await verifyBundledLayr(workspace, repository);
}

// Also used by both package checks. Execute only after the hash has matched.
/** `layr --version` prints `layr version X`, optionally followed by ` (build date)`
 * (layr #46). Same rule as `version_matches` in runtime-services layr_native/install.rs. */
export function layrVersionMatches(output: string, version: string) {
  const line = `layr version ${version}`;
  if (output === line) return true;
  const build = output.startsWith(line) ? output.slice(line.length) : "";
  return build.startsWith(" (") && build.endsWith(")");
}

export async function verifyBundledLayr(
  workspace: string,
  repository = repositoryRoot,
) {
  const { version, manifestName, versionTimeoutMs } =
    await layrBundleContract(repository);
  const binary = path.join(workspace, "bin/layr");
  await access(binary, constants.X_OK);
  const manifest = JSON.parse(
    await readFile(path.join(workspace, "bin", manifestName), "utf8"),
  ) as { version?: string; sha256?: string };
  if (manifest.version !== version)
    throw new Error(`Expected Layr ${version} in the package manifest.`);
  if (manifest.sha256 !== (await sha256File(binary)))
    throw new Error("Packaged Layr hash mismatch.");
  const actual = execFileSync(binary, ["--version"], {
    encoding: "utf8",
    timeout: versionTimeoutMs,
  }).trim();
  if (!layrVersionMatches(actual, version))
    throw new Error(`Expected Layr ${version}, found ${actual}.`);
  return version;
}
