// Keep one certificate identity across local application updates.
import { execFileSync, spawnSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { homedir } from "node:os";
import path from "node:path";
import { pathToFileURL } from "node:url";

function configuration(env = process.env) {
  if (env.CODEX_STUDIO_SIGNING_IDENTITY)
    return {
      identity: env.CODEX_STUDIO_SIGNING_IDENTITY,
      keychain: env.CODEX_STUDIO_SIGNING_KEYCHAIN,
    };
  try {
    return JSON.parse(
      readFileSync(
        path.join(homedir(), ".config/codex-studio/signing.json"),
        "utf8",
      ),
    );
  } catch (error) {
    if (error.code !== "ENOENT") throw error;
    return {};
  }
}
function validate(identity) {
  if (typeof identity !== "string" || !/^[a-fA-F0-9]{40}$/.test(identity))
    throw new Error(
      "Configure a persistent Studio signing certificate. Ad hoc signing loses macOS permissions after updates.",
    );
}
export function signingIdentity(env = process.env) {
  const config = configuration(env);
  validate(config.identity);
  if (config.keychain && config.passwordFile) {
    const result = spawnSync(
      "security",
      [
        "unlock-keychain",
        "-p",
        readFileSync(config.passwordFile, "utf8").trim(),
        config.keychain,
      ],
      { stdio: "pipe" },
    );
    if (result.status !== 0)
      throw new Error(
        "Could not unlock the dedicated Studio signing keychain.",
      );
  }
  const identities = execFileSync(
    "security",
    [
      "find-identity",
      "-p",
      "codesigning",
      ...(config.keychain ? [config.keychain] : []),
    ],
    { encoding: "utf8" },
  );
  if (!identities.toUpperCase().includes(config.identity.toUpperCase()))
    throw new Error(
      "The configured Studio signing certificate is unavailable in Keychain.",
    );
  return config.identity;
}
export function signCode(target, identity = signingIdentity(), extra = []) {
  validate(identity);
  const config = configuration();
  const keychain =
    config.identity?.toUpperCase() === identity.toUpperCase() &&
    config.keychain;
  execFileSync(
    "codesign",
    [
      "--force",
      ...(keychain ? ["--keychain", keychain] : []),
      "--sign",
      identity,
      ...extra,
      target,
    ],
    { stdio: "inherit" },
  );
}
export function signApplication(application, identity = signingIdentity()) {
  signCode(application, identity, [
    "--deep",
    "--identifier",
    "local.codex.agents",
  ]);
  execFileSync("codesign", ["--verify", "--deep", "--strict", application], {
    stdio: "inherit",
  });
}
if (
  process.argv[1] &&
  import.meta.url === pathToFileURL(path.resolve(process.argv[1])).href
) {
  if (process.argv.length !== 3)
    throw new Error("Supply the application bundle path.");
  signApplication(path.resolve(process.argv[2]));
}
