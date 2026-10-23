// `just signing-check`: prove the package signing setup works in seconds, before
// a long build. It loads the same configuration as signing.mjs, unlocks the
// keychain the same way and looks up the identity. It prints the keychain path
// and the certificate fingerprint. It never prints the password or key material.
import { readFileSync, statSync } from "node:fs";
import path from "node:path";
import { pathToFileURL } from "node:url";
import { signingConfigPath, signingIdentity, validate } from "./signing.mjs";

export const signingSetupDocument =
  "workspaces/client/apps/native/README.md#signing-setup";

class CheckFailure extends Error {}

function statFile(file, label) {
  let stat;
  try {
    stat = statSync(file);
  } catch (error) {
    throw new CheckFailure(
      error.code === "ENOENT"
        ? `${label} ${file} does not exist.`
        : `${label} ${file} is not readable: ${error.code}.`,
    );
  }
  if (!stat.isFile()) throw new CheckFailure(`${label} ${file} is not a file.`);
  return stat;
}

// Group and other must have no access to a file that names or holds a secret.
function requirePrivate(file, label) {
  const mode = statFile(file, label).mode & 0o777;
  if (mode & 0o077)
    throw new CheckFailure(
      `${label} ${file} has mode 0${mode.toString(8)}. Run: chmod 600 ${JSON.stringify(file)}`,
    );
}

function loadSettings(env) {
  if (env.CODEX_STUDIO_SIGNING_IDENTITY)
    return {
      source: "the CODEX_STUDIO_SIGNING_IDENTITY environment variable",
      settings: {
        identity: env.CODEX_STUDIO_SIGNING_IDENTITY,
        keychain: env.CODEX_STUDIO_SIGNING_KEYCHAIN,
      },
    };
  const file = signingConfigPath();
  requirePrivate(file, "Signing configuration");
  let settings;
  try {
    settings = JSON.parse(readFileSync(file, "utf8"));
  } catch (error) {
    throw new CheckFailure(
      `Signing configuration ${file} is not valid JSON: ${error.message}`,
    );
  }
  if (!settings || typeof settings !== "object" || Array.isArray(settings))
    throw new CheckFailure(`Signing configuration ${file} must be an object.`);
  return { source: file, settings };
}

// Return { keychain, identity } or throw a CheckFailure with the exact reason.
export function checkSigning(env = process.env) {
  const { source, settings } = loadSettings(env);
  for (const key of ["identity", "keychain", "passwordFile"])
    if (settings[key] !== undefined && typeof settings[key] !== "string")
      throw new CheckFailure(`"${key}" in ${source} must be a string.`);
  try {
    validate(settings.identity);
  } catch {
    throw new CheckFailure(
      `"identity" in ${source} must be a 40 character hexadecimal certificate fingerprint.`,
    );
  }
  if (settings.passwordFile && !settings.keychain)
    throw new CheckFailure(
      `"passwordFile" in ${source} needs a "keychain" path.`,
    );
  if (settings.keychain) statFile(settings.keychain, "Keychain file");
  if (settings.keychain && settings.passwordFile)
    requirePrivate(settings.passwordFile, "Password file");
  // Unlock and identity lookup are the same code the build runs.
  return { keychain: settings.keychain, identity: signingIdentity(env) };
}

export function signingCheckReport(env = process.env) {
  try {
    const { keychain, identity } = checkSigning(env);
    return {
      ok: true,
      line: `PASS: keychain ${keychain ?? "(default search list)"} identity ${identity}`,
    };
  } catch (error) {
    return {
      ok: false,
      line: `FAIL: ${error.message}\nSee ${signingSetupDocument}`,
    };
  }
}

if (
  process.argv[1] &&
  import.meta.url === pathToFileURL(path.resolve(process.argv[1])).href
) {
  const { ok, line } = signingCheckReport();
  (ok ? console.log : console.error)(line);
  process.exitCode = ok ? 0 : 1;
}
