import { validate, validateSigningConfiguration } from "./signing-settings.ts";
import type { SigningConfiguration } from "./signing-settings.ts";
export { validate } from "./signing-settings.ts";
// Keep one certificate identity across local application updates.
import { execFileSync, spawnSync } from "node:child_process";
import {
  readFileSync,
  mkdirSync,
  chmodSync,
  existsSync,
  openSync,
  closeSync,
  readdirSync,
} from "node:fs";
import { homedir } from "node:os";
import path from "node:path";
import { pathToFileURL, fileURLToPath } from "node:url";

// Owner of the signing configuration location. Read it at call time so a test
// can set HOME.
export function signingConfigPath() {
  return path.join(homedir(), ".config/codex-studio/signing.json");
}
export function configuration(env = process.env): SigningConfiguration {
  if (env.CODEX_STUDIO_SIGNING_IDENTITY)
    return {
      identity: env.CODEX_STUDIO_SIGNING_IDENTITY,
      keychain: env.CODEX_STUDIO_SIGNING_KEYCHAIN,
    };
  try {
    const value: unknown = JSON.parse(
      readFileSync(signingConfigPath(), "utf8"),
    );
    validateSigningConfiguration(value, signingConfigPath());
    return value;
  } catch (error) {
    if (!(error instanceof Error && "code" in error && error.code === "ENOENT"))
      throw error;
    return {};
  }
}
function privateArgument(value: unknown) {
  if (typeof value !== "string" || /[\0\r\n]/.test(value))
    throw new Error("Invalid dedicated Studio signing input.");
  return `"${value.replace(/[\\"]/g, "\\$&")}"`;
}
// The first line of the security stderr names the cause. Keep it short, free of
// control characters and free of the password. Never include other output.
function unlockReason(
  result: { error?: unknown; status?: number | null; stderr?: string },
  keychain: string,
  password: string,
) {
  const secrets = [password, privateArgument(password).slice(1, -1)].filter(
    Boolean,
  );
  const clean = (text: string) =>
    secrets
      .reduce((value, secret) => value.split(secret).join("[redacted]"), text)
      .replaceAll(/\p{Cc}/gu, " ")
      .trim();
  if (result.error)
    return clean(
      String(
        result.error instanceof Error
          ? "code" in result.error
            ? result.error.code
            : result.error.message
          : result.error,
      ),
    );
  const line = String(result.stderr ?? "")
    .split("\n")
    .map((text) => text.trim())
    .find(Boolean);
  if (!line) return `security exited with status ${result.status}`;
  let reason = line.replace(/^security:\s*/, "");
  const prefix = `SecKeychainUnlock ${keychain}: `;
  if (reason.startsWith(prefix)) reason = reason.slice(prefix.length);
  return clean(reason).slice(0, 300).replace(/\.$/, "");
}
export function signingIdentity(env = process.env) {
  const config = configuration(env);
  validate(config.identity);
  if (config.keychain && config.passwordFile) {
    const password = readFileSync(config.passwordFile, "utf8").trim();
    const input = `unlock-keychain -p ${privateArgument(password)} ${privateArgument(config.keychain)}\n`;
    // security has a 4096-byte line buffer. Never allow a second parsed command.
    if (Buffer.byteLength(input, "utf8") > 4095)
      throw new Error("Invalid dedicated Studio signing input.");
    const failure = (reason: string) =>
      new Error(
        `Could not unlock the dedicated Studio signing keychain ${config.keychain}: ${reason}.`,
      );
    let result;
    try {
      result = spawnSync("/usr/bin/security", ["-i"], {
        input,
        encoding: "utf8",
        stdio: "pipe",
        timeout: 10_000,
      });
    } catch (error) {
      throw failure(unlockReason({ error }, config.keychain, password));
    }
    if (result.status !== 0 || result.error)
      throw failure(unlockReason(result, config.keychain, password));
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
      `The configured Studio signing certificate ${config.identity} is not in ${config.keychain ? `keychain ${config.keychain}` : "the default keychain search list"}.`,
    );
  return config.identity;
}
export function signCode(
  target: string,
  identity = signingIdentity(),
  extra: string[] = [],
) {
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
export function signLinuxVM(target: string, identity = signingIdentity()) {
  signCode(target, identity, [
    "--identifier",
    "local.codex.agents.linux-vm",
    "--entitlements",
    fileURLToPath(
      new URL("./native/linux-vm/entitlements.plist", import.meta.url),
    ),
  ]);
  execFileSync("codesign", ["--verify", "--strict", target], {
    stdio: "inherit",
    timeout: 10_000,
  });
}
// Every folder with Python modules gets an empty read-only __pycache__.
function guardBytecode(directory: string) {
  const entries = readdirSync(directory, { withFileTypes: true });
  if (entries.some((entry) => entry.isFile() && entry.name.endsWith(".py"))) {
    const cache = path.join(directory, "__pycache__");
    mkdirSync(cache, { recursive: true });
    chmodSync(cache, 0o555);
  }
  for (const entry of entries) {
    if (
      entry.isDirectory() &&
      entry.name !== "__pycache__" &&
      entry.name !== "node_modules"
    )
      guardBytecode(path.join(directory, entry.name));
  }
}
export function signApplication(
  application: string,
  identity = signingIdentity(),
) {
  // External Python callers (a VM helper child, the host_exec service) can import
  // these modules without -B. Cache writes would add unsealed resources after
  // installation, invalidate the bundle, and fail the update of a paired Mac.
  const workspace = path.join(application, "Contents/Resources/workspace");
  if (existsSync(workspace)) guardBytecode(workspace);
  const scripts = path.join(workspace, "scripts");
  if (existsSync(scripts)) {
    // The updater opens this lease on its first tick. Create it before sealing
    // resources, and preserve the inode and contents of an existing lease.
    closeSync(openSync(path.join(scripts, ".studio-update.lock"), "a"));
  }
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
  if (process.argv.length !== 3 || !process.argv[2])
    throw new Error("Supply the application bundle path.");
  signApplication(path.resolve(process.argv[2]));
}
