// Admission for a fresh profile created by Studio's existing Add Claude flow.
// Reads application metadata only. Never reads native credentials or Keychain.
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { execFileSync } from "node:child_process";
import { randomUUID } from "node:crypto";

const VERSION = 1;
const UUID =
  /^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;
/** Maximum new metadata rows per family since reservation; unit: rows. */
const MAX_NEW_METADATA_ROWS = 100_000;
const FAMILIES = [
  ["canvas.sqlite3", "runtime_execution_runs"],
  ["canvas.sqlite3", "runtime_account_transfers"],
  ["analytics.sqlite3", "analytics_agents"],
  ["analytics.sqlite3", "analytics_usage"],
];
/** Private ownership records, not native profile storage. Unit: absolute directory. */
export const OWNERSHIP_DIRECTORY = path.join(
  os.homedir(),
  ".config/codex-studio/canary-claude-keychain",
);
const fail = () => {
  throw new Error(
    "Cannot prove a fresh, excluded Studio canary profile. Keep the existing profile unchanged.",
  );
};
function identity(file) {
  const s = fs.lstatSync(file, { bigint: true });
  if (s.isSymbolicLink() || s.uid !== BigInt(process.getuid())) fail();
  return { dev: String(s.dev), ino: String(s.ino) };
}
function directory(folder) {
  const s = fs.lstatSync(folder);
  if (
    !s.isDirectory() ||
    s.isSymbolicLink() ||
    s.uid !== process.getuid() ||
    s.mode & 0o077 ||
    fs.realpathSync(folder) !== folder
  )
    fail();
  return identity(folder);
}
function metadata(file) {
  directory(path.dirname(file));
  const fd = fs.openSync(file, fs.constants.O_RDONLY | fs.constants.O_NOFOLLOW);
  try {
    const s = fs.fstatSync(fd);
    if (
      !s.isFile() ||
      s.uid !== process.getuid() ||
      s.mode & 0o077 ||
      s.size > 16 * 1024 * 1024
    )
      fail();
    try {
      return JSON.parse(fs.readFileSync(fd, "utf8"));
    } catch {
      return fail();
    }
  } finally {
    fs.closeSync(fd);
  }
}
function recordLocation(file, storage) {
  if (
    path.dirname(file) !== storage ||
    !path.isAbsolute(file) ||
    path.resolve(file) !== file
  )
    fail();
  // Create only ownership storage. No live registry or native profile mutation.
  if (!fs.existsSync(storage))
    fs.mkdirSync(storage, { recursive: true, mode: 0o700 });
  directory(storage);
}
function syncFolder(folder) {
  const fd = fs.openSync(folder, "r");
  try {
    fs.fsyncSync(fd);
  } finally {
    fs.closeSync(fd);
  }
}
function save(file, record, replace) {
  const temporary = replace ? `${file}.${randomUUID()}.next` : file;
  const fd = fs.openSync(temporary, "wx", 0o600);
  try {
    fs.writeFileSync(fd, JSON.stringify(record));
    fs.fsyncSync(fd);
  } finally {
    fs.closeSync(fd);
  }
  if (replace) fs.renameSync(temporary, file);
  syncFolder(path.dirname(file));
}
function equal(a, b) {
  return JSON.stringify(a) === JSON.stringify(b);
}
function registry(live) {
  return metadata(path.join(live, "accounts/registry.json"));
}
function baseline(live) {
  const root = path.join(live, "claude-logins");
  const names = fs.existsSync(root)
    ? (directory(root), fs.readdirSync(root).sort())
    : [];
  const accounts = registry(live).accounts;
  if (!accounts || typeof accounts !== "object" || Array.isArray(accounts))
    fail();
  return {
    names,
    accounts: Object.keys(accounts).sort(),
    configs: Object.values(accounts)
      .map((a) => a?.claudeOptions?.configDir)
      .filter((x) => typeof x === "string")
      .sort(),
  };
}
function load(file, live, storage) {
  recordLocation(file, storage);
  const r = metadata(file);
  if (
    r.version !== VERSION ||
    r.source !== "studio-add" ||
    !UUID.test(r.id) ||
    r.uid !== process.getuid() ||
    r.liveState !== live ||
    !equal(r.stateIdentity, directory(live)) ||
    !Array.isArray(r.baseline?.names) ||
    !Array.isArray(r.baseline?.accounts) ||
    !Array.isArray(r.baseline?.configs) ||
    !r.watermarks ||
    !["reserved", "bound"].includes(r.phase)
  )
    fail();
  return r;
}
/** A retry returns the original baseline. It never refreshes admission evidence. */
export function reserveStudioProfile(
  file,
  live,
  { storage = OWNERSHIP_DIRECTORY } = {},
) {
  recordLocation(file, storage);
  if (fs.existsSync(file)) return load(file, live, storage);
  // Watermarks come BEFORE the registry/config baseline. A profile created during
  // reservation is therefore either already excluded or covered by the new-row range.
  const watermarks = Object.fromEntries(
    FAMILIES.map(([name, table]) => [
      table,
      {
        name,
        identity: identity(path.join(live, name)),
        row: maxRow(live, name, table),
      },
    ]),
  );
  const r = {
    version: VERSION,
    source: "studio-add",
    id: randomUUID(),
    uid: process.getuid(),
    liveState: live,
    stateIdentity: directory(live),
    watermarks,
    baseline: baseline(live),
    phase: "reserved",
  };
  save(file, r, false);
  return r;
}
function sqlite(live, name, sql) {
  const file = path.join(live, name);
  const s = fs.lstatSync(file);
  if (!s.isFile() || s.isSymbolicLink() || s.uid !== process.getuid()) fail();
  try {
    const out = execFileSync(
      "/usr/bin/sqlite3",
      [
        "-batch",
        "-init",
        "/dev/null",
        "-readonly",
        file,
        `PRAGMA query_only=ON; BEGIN; ${sql} COMMIT;`,
      ],
      {
        encoding: "utf8",
        timeout: 15_000,
        stdio: ["ignore", "pipe", "pipe"],
        maxBuffer: 1024,
      },
    );
    return out.trim();
  } catch {
    fail();
  }
}
function maxRow(live, name, table) {
  const value = Number(
    sqlite(live, name, `SELECT coalesce(max(rowid),0) FROM ${table};`),
  );
  if (!Number.isSafeInteger(value) || value < 0) fail();
  return value;
}
function absent(live, name, table, sql) {
  // Refuse a schema/query drift that would scan the old state or decode old bodies.
  const plan = sqlite(live, name, "EXPLAIN QUERY PLAN " + sql);
  if (!plan.includes(`SEARCH ${table} USING`) || plan.includes(`SCAN ${table}`))
    fail();
  if (sqlite(live, name, sql) !== "0") fail();
}
/** Metadata owners: runtime_agents, runtime_execution_runs, analytics capture, supervisor handles.
 * Keep tombstones and historic executions in the query. No transcript/body reads. */
function unused(r, key) {
  const live = r.liveState;
  // The source owner is agent_columns.rs / AgentIndexedReads::ACCOUNT_INDEX.
  // A legacy schema without this keyed read is not sufficient admission evidence.
  absent(
    live,
    "canvas.sqlite3",
    "runtime_agents",
    `SELECT EXISTS(SELECT 1 FROM runtime_agents WHERE account_key='${key}');`,
  );
  for (const [name, table] of FAMILIES) {
    const mark = r.watermarks[table];
    if (
      !mark ||
      mark.name !== name ||
      !Number.isSafeInteger(mark.row) ||
      mark.row < 0 ||
      !equal(mark.identity, identity(path.join(live, name)))
    )
      fail();
    const end = maxRow(live, name, table);
    if (end < mark.row || end - mark.row > MAX_NEW_METADATA_ROWS) fail();
    const match =
      table === "runtime_execution_runs"
        ? `account='${key}'`
        : `json_extract(record,'$.${table === "runtime_account_transfers" ? "targetAccountKey" : "accountKey"}')='${key}'`;
    absent(
      live,
      name,
      table,
      `SELECT EXISTS(SELECT 1 FROM ${table} WHERE rowid>${mark.row} AND rowid<=${end} AND ${match});`,
    );
  }
  absent(
    live,
    "supervisor.sqlite3",
    "handles",
    // AccountRegistry starts SDK workers for empty pages and model metadata.
    // A closed handle alone does not prove chat or model input. All such input
    // footprints remain forbidden; a live handle still shares this profile.
    `SELECT EXISTS(SELECT 1 FROM handles WHERE id>='account:${key}' AND id<'account:${key};' AND closed_at IS NULL);`,
  );
}
function binaryIdentity(file) {
  if (!path.isAbsolute(file)) fail();
  const resolved = fs.realpathSync(file),
    s = fs.statSync(file, { bigint: true });
  if (!s.isFile() || !(s.mode & 0o111n)) fail();
  return {
    path: file,
    resolved,
    dev: String(s.dev),
    ino: String(s.ino),
    size: String(s.size),
    mtime: String(s.mtimeNs),
  };
}
function admission(r, login) {
  if (
    !UUID.test(login) ||
    r.baseline.names.includes(login + ".json") ||
    r.baseline.names.includes("config-" + login)
  )
    fail();
  const live = r.liveState,
    config = path.join(live, "claude-logins", "config-" + login);
  directory(path.dirname(config));
  if (r.baseline.configs.includes(config)) fail();
  const configIdentity = directory(config);
  const receipt = metadata(path.join(live, "claude-logins", login + ".json"));
  const v = receipt.receipt,
    m = receipt.metadata,
    key = v?.accountKey;
  if (
    v?.requestId !== login ||
    v.status !== "ready" ||
    v.chatsRefreshed !== true ||
    m?.flow !== "add" ||
    m.cleanupDone !== true ||
    m.configDir !== config ||
    !/^claude-profile-[0-9a-f]{20}$/.test(key) ||
    r.baseline.accounts.includes(key)
  )
    fail();
  const all = registry(live),
    account = all.accounts?.[key];
  if (
    !account ||
    account.id !== key ||
    account.provider !== "claude" ||
    account.deleted !== true ||
    all.defaultAccountKey === key ||
    account.status !== "ready" ||
    typeof account.email !== "string" ||
    !account.email ||
    v.email !== account.email ||
    account.accountId !== `claude:${account.email}` ||
    typeof account._credentialIdentity !== "string" ||
    !account._credentialIdentity ||
    account.home !== config ||
    account.claudeOptions?.configDir !== config ||
    account.claudeOptions?.binaryPath !== m.binaryPath
  )
    fail();
  // Any second registry identity for this directory would still share the login.
  if (
    Object.entries(all.accounts).some(
      ([id, a]) =>
        id !== key &&
        (a?.home === config || a?.claudeOptions?.configDir === config),
    )
  )
    fail();
  unused(r, key);
  return {
    login,
    config,
    configIdentity,
    binary: binaryIdentity(m.binaryPath),
    account: {
      id: key,
      email: account.email,
      accountId: account.accountId,
      credentialIdentity: account._credentialIdentity,
    },
  };
}
export function bindStudioProfile(
  file,
  login,
  { storage = OWNERSHIP_DIRECTORY, liveState } = {},
) {
  recordLocation(file, storage);
  const lock = fs.openSync(file + ".lock", "wx", 0o600);
  try {
    const raw = metadata(file),
      r = load(file, liveState ?? raw.liveState, storage);
    const bound = admission(r, login);
    if (r.phase === "bound") {
      if (!equal(r.binding, bound)) fail();
      return r;
    }
    r.phase = "bound";
    r.binding = bound;
    save(file, r, true);
    return r;
  } finally {
    fs.closeSync(lock);
    fs.unlinkSync(file + ".lock");
  }
}
/** Recheck source records and live exclusion at every canary start. */
export function studioProfile(
  file,
  live,
  { storage = OWNERSHIP_DIRECTORY } = {},
) {
  const r = load(file, live, storage);
  if (r.phase !== "bound") fail();
  const current = admission(r, r.binding?.login);
  if (!equal(current, r.binding)) fail();
  return {
    config: current.config,
    binary: current.binary.path,
    account: {
      ...current.account,
      _credentialIdentity: current.account.credentialIdentity,
    },
  };
}
