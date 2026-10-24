// Native path identity stays exact. Keychain profile provenance belongs to
// studio-claude-profile.mjs; this module also preserves legacy file-mode support.
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

export function fileConfig(config, { home = os.homedir() } = {}) {
  const base = path.join(home, ".config/codex-studio/canary-claude");
  if (
    !path.isAbsolute(config) ||
    config.trim() !== config ||
    path.resolve(config) !== config ||
    config.normalize("NFC") !== config ||
    fs.realpathSync(home) !== home ||
    !(config === base || config.startsWith(base + path.sep))
  )
    throw new Error(
      "Use the dedicated file-mode canary Claude directory without a path alias.",
    );
  let folder = home;
  for (const part of ["", ...path.relative(home, config).split(path.sep)]) {
    folder = path.join(folder, part);
    let stat;
    try {
      stat = fs.lstatSync(folder);
    } catch (error) {
      if (error.code === "ENOENT") return config;
      throw error;
    }
    if (
      !stat.isDirectory() ||
      stat.isSymbolicLink() ||
      stat.uid !== process.getuid() ||
      stat.mode & (folder === config ? 0o077 : 0o022)
    )
      throw new Error(
        "The canary Claude directory must be private, owned, and free of symbolic links.",
      );
  }
  for (const name of [".credentials.json", ".claude.json"]) {
    try {
      const stat = fs.lstatSync(path.join(config, name));
      if (
        !stat.isFile() ||
        stat.isSymbolicLink() ||
        stat.uid !== process.getuid()
      )
        throw new Error(
          "Native canary auth files cannot use a path alias or another owner.",
        );
    } catch (error) {
      if (error.code !== "ENOENT") throw error;
    }
  }
  return config;
}

/** Legacy private account metadata only. Never read a native credential file. */
export function claudeAccount(config) {
  const fd = fs.openSync(
    path.join(config, ".claude.json"),
    fs.constants.O_RDONLY | fs.constants.O_NOFOLLOW,
  );
  try {
    const stat = fs.fstatSync(fd);
    if (
      !stat.isFile() ||
      stat.uid !== process.getuid() ||
      stat.size > 16 * 1024 * 1024
    )
      throw new Error("The canary Claude account metadata is invalid.");
    let account;
    try {
      account = JSON.parse(fs.readFileSync(fd, "utf8")).oauthAccount;
    } catch {
      // JSON errors can quote private source text. Do not propagate the exception.
      throw new Error("The private canary Claude metadata is invalid.");
    }
    if (
      typeof account?.emailAddress !== "string" ||
      !account.emailAddress.trim()
    )
      throw new Error("The canary Claude login has no account email.");
    return account;
  } finally {
    fs.closeSync(fd);
  }
}

export function claudeConfigEnvironment(config) {
  return { CLAUDE_CONFIG_DIR: config, CLAUDE_SECURESTORAGE_CONFIG_DIR: config };
}
