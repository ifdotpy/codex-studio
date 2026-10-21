// Verify SDK file effects independently: canRewind permits partial success.
// The native snapshot/delta format is read only; the SDK performs every rewind.
import fs from "node:fs/promises";
import { createReadStream, constants } from "node:fs";
import { createInterface } from "node:readline";
import { createHash } from "node:crypto";
import path from "node:path";
import os from "node:os";
import { nativeFile } from "./move.mjs";

// Native file-history retention count, from Claude CLI 2.1.302 (dde/Ejt).
// Reconstruct the same retained snapshots before selecting tracked files.
const NATIVE_FILE_SNAPSHOT_COUNT = 100;
const invalid = () =>
  new Error(
    "The SDK file checkpoint cannot be verified. Inspect workspace recovery.",
  );
const own = (object, key) =>
  Object.hasOwn(object, key) ? object[key] : undefined;

async function identity(file) {
  let stat;
  try {
    stat = await fs.lstat(file);
  } catch (error) {
    if (error.code === "ENOENT") return null;
    throw error;
  }
  if (!stat.isFile() || stat.isSymbolicLink() || stat.nlink > 1)
    throw invalid();
  const handle = await fs.open(file, constants.O_RDONLY | constants.O_NOFOLLOW);
  try {
    const opened = await handle.stat();
    if (opened.ino !== stat.ino || opened.dev !== stat.dev) throw invalid();
    const hash = createHash("sha256");
    for await (const bytes of handle.createReadStream({ autoClose: false }))
      hash.update(bytes);
    const after = await handle.stat();
    if (
      after.size !== opened.size ||
      after.mtimeMs !== opened.mtimeMs ||
      after.ctimeMs !== opened.ctimeMs
    )
      throw invalid();
    return { hash: hash.digest("hex"), mode: opened.mode & 0o777 };
  } finally {
    await handle.close();
  }
}
async function safePath(cwd, file) {
  const relative = path.relative(cwd, file);
  if (
    !relative ||
    relative === ".." ||
    relative.startsWith(".." + path.sep) ||
    path.isAbsolute(relative)
  )
    throw invalid();
  let parent = path.dirname(file);
  for (;;) {
    try {
      if ((await fs.realpath(parent)) !== parent) throw invalid();
      break;
    } catch (error) {
      if (error.code !== "ENOENT") throw error;
      parent = path.dirname(parent);
    }
  }
  return identity(file);
}
function same(a, b) {
  return JSON.stringify(a) === JSON.stringify(b);
}

async function nativeBackups(session, messages, messageId, configDir) {
  const file = await nativeFile(
    session.nativeId || session.id,
    configDir,
    session.nativeHistoryCwd || session.cwd,
  );
  const snapshots = new Map(),
    deltas = new Map();
  const input = createReadStream(file);
  const lines = createInterface({ input, crlfDelay: Infinity });
  try {
    for await (const line of lines) {
      const record = JSON.parse(line);
      if (
        record.type?.startsWith("file-history-") &&
        !["file-history-snapshot", "file-history-delta"].includes(record.type)
      )
        throw invalid();
      if (record.type === "file-history-snapshot")
        snapshots.set(record.messageId, record);
      if (record.type === "file-history-delta") {
        const rows = deltas.get(record.messageId) || [];
        rows.push(record);
        deltas.set(record.messageId, rows);
      }
    }
  } finally {
    lines.close();
    input.destroy();
  }
  // SDK history omits some native record types. Do not silently ignore file
  // history attached to an omitted record or a different conversation chain.
  const anchors = new Set(messages.map((message) => message.uuid));
  if (
    [...snapshots.keys(), ...deltas.keys()].some(
      (anchor) => !anchors.has(anchor),
    )
  )
    throw invalid();
  const ordered = [],
    positions = new Map();
  for (const message of messages) {
    const record = snapshots.get(message.uuid);
    if (record) {
      const snapshot = structuredClone(record.snapshot);
      if (
        !snapshot?.messageId ||
        !snapshot.trackedFileBackups ||
        typeof snapshot.trackedFileBackups !== "object"
      )
        throw invalid();
      const index = record.isSnapshotUpdate
        ? positions.get(snapshot.messageId)
        : undefined;
      if (index === undefined) {
        positions.set(snapshot.messageId, ordered.length);
        ordered.push(snapshot);
      } else ordered[index] = snapshot;
    }
    for (const delta of deltas.get(message.uuid) || []) {
      if (
        typeof delta.snapshotMessageId !== "string" ||
        typeof delta.trackingPath !== "string" ||
        !delta.backup
      )
        throw invalid();
      let index = positions.get(delta.snapshotMessageId);
      if (index === undefined) {
        index = ordered.length;
        positions.set(delta.snapshotMessageId, index);
        ordered.push({
          messageId: delta.snapshotMessageId,
          trackedFileBackups: { ...ordered.at(-1)?.trackedFileBackups },
        });
      }
      Object.defineProperty(
        ordered[index].trackedFileBackups,
        delta.trackingPath,
        {
          value: delta.backup,
          enumerable: true,
          configurable: true,
          writable: true,
        },
      );
    }
  }
  const retained = ordered.slice(-NATIVE_FILE_SNAPSHOT_COUNT);
  if (!retained.length) throw invalid();
  // Normalize native absolute and relative tracking names before selecting backups.
  for (const snapshot of retained) {
    const normalized = Object.create(null);
    for (const [name, backup] of Object.entries(snapshot.trackedFileBackups)) {
      const file = path.resolve(session.cwd, name);
      if (own(normalized, file) && !same(normalized[file], backup))
        throw invalid();
      normalized[file] = backup;
    }
    snapshot.trackedFileBackups = normalized;
  }
  const target = retained.findLast(
    (snapshot) => snapshot.messageId === messageId,
  );
  if (!target) throw invalid();
  const files = new Set(
    retained.flatMap((snapshot) => Object.keys(snapshot.trackedFileBackups)),
  );
  return [...files].map((file) => {
    const backup =
      own(target.trackedFileBackups, file) ||
      retained
        .map((snapshot) => own(snapshot.trackedFileBackups, file))
        .find((value) => value?.version === 1);
    if (
      !backup ||
      !(
        backup.backupFileName === null ||
        /^[0-9a-f]{16}@v\d+$/.test(backup.backupFileName)
      )
    )
      throw invalid();
    return { file, backup };
  });
}

export async function prepareFileRestore(
  session,
  messages,
  messageId,
  stateDir,
  configDir = process.env.CLAUDE_CONFIG_DIR ||
    path.join(os.homedir(), ".claude"),
) {
  const cwd = await fs.realpath(session.cwd);
  const entries = await nativeBackups(session, messages, messageId, configDir);
  const files = [];
  for (const { file, backup } of entries) {
    const before = await safePath(cwd, file);
    if (
      backup.realParentDir &&
      path.resolve(backup.realParentDir) !== path.dirname(file)
    )
      throw invalid();
    const expected =
      backup.backupFileName === null
        ? null
        : await identity(
            path.join(
              configDir,
              "file-history",
              session.nativeId || session.id,
              backup.backupFileName,
            ),
          );
    if (backup.backupFileName !== null && !expected) throw invalid();
    files.push({ file, before, expected });
  }
  const parent = path.join(stateDir, "file-rewinds");
  await fs.mkdir(parent, { recursive: true, mode: 0o700 });
  const directory = await fs.mkdtemp(path.join(parent, "before-"));
  for (let i = 0; i < files.length; i++) {
    const entry = files[i];
    if (entry.before) {
      entry.backup = path.join(directory, String(i));
      await fs.copyFile(entry.file, entry.backup, constants.COPYFILE_EXCL);
      await fs.chmod(entry.backup, entry.before.mode);
      const handle = await fs.open(entry.backup, "r");
      try {
        await handle.sync();
      } finally {
        await handle.close();
      }
      if (!same(await identity(entry.backup), entry.before)) throw invalid();
    }
  }
  const plan = { cwd, directory, files };
  const manifest = await fs.open(
    path.join(directory, "manifest.json"),
    "wx",
    0o600,
  );
  try {
    await manifest.writeFile(JSON.stringify(plan));
    await manifest.sync();
  } finally {
    await manifest.close();
  }
  for (const dir of [directory, parent]) {
    const handle = await fs.open(dir, "r");
    try {
      await handle.sync();
    } finally {
      await handle.close();
    }
  }
  await verifyFileRestore(plan, "before");
  return plan;
}
export async function verifyFileRestore(plan, stage) {
  if (!plan || !["before", "expected"].includes(stage)) throw invalid();
  for (const entry of plan.files) {
    if (!same(await safePath(plan.cwd, entry.file), entry[stage]))
      throw new Error(
        stage === "before"
          ? "Files changed before restore. Inspect workspace recovery."
          : "Claude did not restore every tracked file. The original files remain in the before-restore backup.",
      );
  }
}
