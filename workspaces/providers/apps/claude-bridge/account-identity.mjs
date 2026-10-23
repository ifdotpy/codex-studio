// Wire format owner: runtime-services/src/account_driver/claude_identity.rs.
// account-identity.test.mjs checks the shared identity fixtures in both languages.
import fs from "node:fs";
import path from "node:path";
import os from "node:os";

const prefix = "claude:v2:";
const text = (value) =>
  typeof value === "string" && value.trim() ? value : null;
export function credentialIdentity(email, organization, account) {
  organization = text(organization);
  account = text(account);
  return organization || account
    ? prefix + JSON.stringify([email, organization, account])
    : "claude:" + email;
}
function tuple(identity) {
  if (!identity?.startsWith(prefix)) return null;
  try {
    const value = JSON.parse(identity.slice(prefix.length));
    return Array.isArray(value) &&
      value.length === 3 &&
      text(value[0]) &&
      value.slice(1).every((part) => part === null || text(part))
      ? value
      : null;
  } catch {
    return null;
  }
}
export function acceptsIdentity(expected, observed) {
  if (expected === observed) return true;
  const next = tuple(observed);
  if (!next) return false;
  if (expected === "claude:" + next[0]) return true;
  const old = tuple(expected);
  return (
    !!old &&
    old[0] === next[0] &&
    old
      .slice(1)
      .every((part, index) => part === null || part === next[index + 1])
  );
}
function reject(
  reason,
  message = "Claude Code account identity changed. Restore the original login.",
) {
  throw Object.assign(new Error(message), {
    claudeAccountValidationFailed: true,
    data: {
      turnStartOutcome: "not_applied",
      claudePreparationFailure: "account_validation",
      claudeAccountFailure: reason,
    },
  });
}
function readOptional(file) {
  try {
    return fs.readFileSync(file, "utf8");
  } catch (error) {
    if (error.code === "ENOENT") return null;
    throw error;
  }
}
export function createAccountCheck({
  expectedEmail,
  expectedIdentity = "claude:" + expectedEmail,
  identityPath,
  requireSubscription = true,
  nativePath = path.join(
    process.env.CLAUDE_CONFIG_DIR || os.homedir(),
    ".claude.json",
  ),
}) {
  let pinned = expectedIdentity;
  return function checkAccount(account) {
    if (!expectedEmail)
      reject(
        "expected_identity",
        "Claude Code account identity is not configured. No input was submitted.",
      );
    if (!account || typeof account !== "object" || Array.isArray(account))
      reject(
        "account_metadata",
        "Claude Code account metadata is missing. No input was submitted.",
      );
    if (typeof account.email !== "string" || !account.email.trim())
      reject(
        "email_metadata",
        "Claude Code email metadata is missing. No input was submitted.",
      );
    if (account.email !== expectedEmail)
      reject(
        "identity_mismatch",
        "Claude Code account or subscription changed: expected " +
          expectedEmail +
          ", got " +
          account.email +
          ". Restore the original login.",
      );
    if (typeof account.apiProvider !== "string" || !account.apiProvider)
      reject(
        "provider_metadata",
        "Claude Code API provider metadata is missing. No input was submitted.",
      );
    if (account.apiProvider !== "firstParty")
      reject(
        "provider_mismatch",
        "Claude Code API provider is " +
          account.apiProvider +
          ", expected firstParty. Restore the original login.",
      );
    if (
      requireSubscription &&
      (typeof account.subscriptionType !== "string" ||
        !account.subscriptionType.trim())
    )
      reject(
        "subscription_metadata",
        "Claude Code subscription metadata is missing. No input was submitted.",
      );
    if (account.apiKeySource && account.apiKeySource !== "none")
      reject(
        "api_key_source",
        "Claude Code uses an API key instead of the configured subscription. Restore the original login.",
      );

    // SDK releases can omit UUIDs. The organization display name is not an ID.
    let organization = text(account.organizationUuid) || text(account.orgId);
    let uuid = text(account.accountUuid);
    try {
      const native = readOptional(nativePath);
      const profile = native === null ? null : JSON.parse(native).oauthAccount;
      if (profile) {
        if (
          profile.emailAddress !== account.email ||
          (organization &&
            text(profile.organizationUuid) &&
            organization !== profile.organizationUuid) ||
          (uuid && text(profile.accountUuid) && uuid !== profile.accountUuid)
        )
          reject("identity_mismatch");
        organization ||= text(profile.organizationUuid);
        uuid ||= text(profile.accountUuid);
      }
      const observed = credentialIdentity(account.email, organization, uuid);
      if (identityPath) {
        const stored = readOptional(identityPath);
        if (stored !== null) {
          if (acceptsIdentity(pinned, stored)) pinned = stored;
          else if (!acceptsIdentity(stored, pinned))
            reject("identity_mismatch");
        }
      }
      if (!acceptsIdentity(pinned, observed)) reject("identity_mismatch");
      if (
        observed !== pinned ||
        (identityPath &&
          observed.startsWith(prefix) &&
          readOptional(identityPath) !== observed)
      ) {
        if (identityPath) {
          fs.mkdirSync(path.dirname(identityPath), { recursive: true });
          // The bridge owns this account directory. Synchronous replacement
          // commits the pin before any query can submit input or return usage.
          const temporary = identityPath + "." + process.pid + ".tmp";
          const file = fs.openSync(temporary, "w", 0o600);
          try {
            fs.writeFileSync(file, observed);
            fs.fsyncSync(file);
          } finally {
            fs.closeSync(file);
          }
          fs.renameSync(temporary, identityPath);
          const directory = fs.openSync(path.dirname(identityPath), "r");
          try {
            fs.fsyncSync(directory);
          } finally {
            fs.closeSync(directory);
          }
        }
        pinned = observed;
      }
      return Object.assign(account, {
        organizationUuid: organization || undefined,
        accountUuid: uuid || undefined,
        _credentialIdentity: observed,
      });
    } catch (error) {
      if (error.claudeAccountValidationFailed) throw error;
      reject(
        "organization_metadata",
        "Claude Code organization metadata is unavailable. No input was submitted.",
      );
    }
  };
}
