// Canary profile paths own the native Keychain service identity. Never canonicalize
// an alias into another profile: Claude hashes the supplied directory text.
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

/** Marker for a profile created exclusively for native canary authentication. */
const MARKER = "canary-auth.json";
const VERSION = 1;

function privateJSON(file) {
  try {
    return JSON.parse(fs.readFileSync(file, "utf8"));
  } catch {
    // JSON errors can quote private source text. Never propagate that exception.
    throw new Error("The private canary Claude metadata is invalid.");
  }
}

export function defaultKeychainConfig(home = os.homedir()) {
  return path.join(home, ".config/codex-studio/canary-claude-keychain/native");
}

function scope(config, home, keychain) {
  const base = path.join(
    home,
    ".config/codex-studio",
    keychain ? "canary-claude-keychain" : "canary-claude",
  );
  if (
    !path.isAbsolute(config) ||
    config.trim() !== config ||
    path.resolve(config) !== config ||
    config.normalize("NFC") !== config ||
    fs.realpathSync(home) !== home ||
    !(config === base || config.startsWith(base + path.sep))
  )
    throw new Error(
      "Use the dedicated canary Claude directory without a path alias.",
    );
}

function directory(folder, { privateMode = false } = {}) {
  const stat = fs.lstatSync(folder);
  if (
    !stat.isDirectory() ||
    stat.isSymbolicLink() ||
    stat.uid !== process.getuid() ||
    stat.mode & (privateMode ? 0o077 : 0o022)
  )
    throw new Error(
      "The canary Claude directory must be private, owned, and free of symbolic links.",
    );
}

function parents(config, home, create, allowMissing = false) {
  directory(home);
  const parts = path.relative(home, config).split(path.sep);
  let folder = home;
  for (const part of parts) {
    folder = path.join(folder, part);
    if (allowMissing && !fs.existsSync(folder)) {
      try {
        fs.lstatSync(folder);
      } catch (error) {
        if (error.code === "ENOENT") return;
        throw error;
      }
    }
    if (create && !fs.existsSync(folder)) fs.mkdirSync(folder, { mode: 0o700 });
    directory(folder, { privateMode: folder === config });
  }
}

function marker(config) {
  const file = path.join(config, MARKER);
  const descriptor = fs.openSync(
    file,
    fs.constants.O_RDONLY | fs.constants.O_NOFOLLOW,
  );
  try {
    const stat = fs.fstatSync(descriptor);
    if (
      !stat.isFile() ||
      stat.uid !== process.getuid() ||
      stat.mode & 0o077 ||
      stat.size > 4096
    )
      throw new Error("The canary Claude profile marker is not private.");
    const record = privateJSON(descriptor);
    if (
      record.version !== VERSION ||
      record.config !== config ||
      record.uid !== process.getuid() ||
      record.auth !== "native-keychain"
    )
      throw new Error("The canary Claude profile marker has another identity.");
  } finally {
    fs.closeSync(descriptor);
  }
}

function authPaths(config) {
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
}

export function keychainConfig(config, { home = os.homedir() } = {}) {
  scope(config, home, true);
  parents(config, home, false);
  marker(config);
  authPaths(config);
  return config;
}

export function fileConfig(config, { home = os.homedir() } = {}) {
  scope(config, home, false);
  parents(config, home, false, true);
  authPaths(config);
  return config;
}

/** Create a new empty profile, or verify a profile this helper already created. */
export function prepareKeychainConfig(config, { home = os.homedir() } = {}) {
  scope(config, home, true);
  if (fs.existsSync(config)) return keychainConfig(config, { home });
  parents(path.dirname(config), home, true);
  fs.mkdirSync(config, { mode: 0o700 });
  const file = fs.openSync(path.join(config, MARKER), "wx", 0o600);
  try {
    fs.writeFileSync(
      file,
      JSON.stringify({
        version: VERSION,
        auth: "native-keychain",
        config,
        uid: process.getuid(),
      }),
    );
    fs.fsyncSync(file);
  } finally {
    fs.closeSync(file);
  }
  const folder = fs.openSync(config, "r");
  try {
    fs.fsyncSync(folder);
  } finally {
    fs.closeSync(folder);
  }
  return keychainConfig(config, { home });
}

/** Metadata only. Do not read native credential files or query Keychain values. */
export function claudeAccount(config) {
  const file = fs.openSync(
    path.join(config, ".claude.json"),
    fs.constants.O_RDONLY | fs.constants.O_NOFOLLOW,
  );
  try {
    const stat = fs.fstatSync(file);
    if (
      !stat.isFile() ||
      stat.uid !== process.getuid() ||
      stat.size > 16 * 1024 * 1024
    )
      throw new Error("The canary Claude account metadata is invalid.");
    const account = privateJSON(file).oauthAccount;
    if (
      typeof account?.emailAddress !== "string" ||
      !account.emailAddress.trim()
    )
      throw new Error("The canary Claude login has no account email.");
    return account;
  } finally {
    fs.closeSync(file);
  }
}

export function claudeConfigEnvironment(config) {
  return { CLAUDE_CONFIG_DIR: config, CLAUDE_SECURESTORAGE_CONFIG_DIR: config };
}

/** Exact native full-login environment. No inherited token, provider, or debug keys. */
export function nativeLoginSpec({
  config,
  home = os.homedir(),
  tmp,
  profile,
  executable,
  proxyURL,
}) {
  keychainConfig(config, { home });
  if (!path.isAbsolute(executable))
    throw new Error("Use an absolute native Claude executable path.");
  return {
    file: "/usr/bin/sandbox-exec",
    args: ["-f", profile, executable, "auth", "login", "--claudeai"],
    cwd: config,
    env: {
      HOME: home,
      USER: os.userInfo().username,
      LOGNAME: os.userInfo().username,
      SHELL: "/bin/zsh",
      TMPDIR: tmp + path.sep,
      PATH: "/usr/bin:/bin:/usr/sbin:/sbin",
      SSL_CERT_FILE: "/etc/ssl/cert.pem",
      HTTPS_PROXY: proxyURL,
      HTTP_PROXY: proxyURL,
      ALL_PROXY: proxyURL,
      https_proxy: proxyURL,
      http_proxy: proxyURL,
      NO_PROXY: "localhost,127.0.0.1",
      no_proxy: "localhost,127.0.0.1",
      ...claudeConfigEnvironment(config),
    },
  };
}
