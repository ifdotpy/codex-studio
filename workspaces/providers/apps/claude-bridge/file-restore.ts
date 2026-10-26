import { isRecord } from "./move.ts";
import type { SessionMessage } from "@anthropic-ai/claude-agent-sdk";
import type {
  Session,
  FileRestorePlan,
  ControlReceipt,
} from "./session-state.ts";
type RestoreSession = Pick<
  Session,
  "id" | "nativeId" | "cwd" | "nativeHistoryCwd"
>;
export type NativeBackup = {
  backupFileName: string | null;
  version?: number;
  realParentDir?: string;
};
export type NativeSnapshot = {
  messageId: string;
  trackedFileBackups: Record<string, NativeBackup>;
};
export type NativeFileRecord = {
  type?: string;
  messageId: string;
  snapshot?: NativeSnapshot;
  isSnapshotUpdate?: boolean;
  snapshotMessageId?: string;
  trackingPath?: string;
  backup?: NativeBackup;
};
// Verify SDK file effects independently: canRewind permits partial success.
// The native snapshot/delta format is read only; the SDK performs every rewind.
import fs from "node:fs/promises";
import { createReadStream, constants } from "node:fs";
import { createInterface } from "node:readline";
import { createHash } from "node:crypto";
import path from "node:path";
import os from "node:os";
import { nativeFile } from "./move.ts";

// Native file-history retention count, from Claude CLI 2.1.302 (dde/Ejt).
// Reconstruct the same retained snapshots before selecting tracked files.
const NATIVE_FILE_SNAPSHOT_COUNT = 100;
const invalid = () =>
  new Error(
    "The SDK file checkpoint cannot be verified. Inspect workspace recovery.",
  );
const own = <T>(object: Record<string, T>, key: string) =>
  Object.hasOwn(object, key) ? object[key] : undefined;

async function identity(file: string) {
  let stat;
  try {
    stat = await fs.lstat(file);
  } catch (error) {
    if (
      (error && typeof error === "object" && "code" in error
        ? error.code
        : undefined) === "ENOENT"
    )
      return null;
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
    return {
      hash: hash.digest("hex"),
      mode: opened.mode & 0o777,
      bytes: opened.size,
    };
  } finally {
    await handle.close();
  }
}
async function safePath(cwd: string, file: string) {
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
      if (!isRecord(error) || error.code !== "ENOENT") throw error;
      parent = path.dirname(parent);
    }
  }
  return identity(file);
}
// This manifest is written by prepareFileRestoreOwned in this bridge and compared
// byte-for-byte as JSON against the durable receipt before any deletion.
function parseBackupManifest(text: string): FileRestorePlan {
  return JSON.parse(text);
}
function same(a: unknown, b: unknown) {
  return JSON.stringify(a) === JSON.stringify(b);
}

async function nativeBackups(
  session: RestoreSession,
  messages: Pick<SessionMessage, "uuid">[],
  messageId: string,
  configDir: string,
) {
  const file = await nativeFile(
    session.nativeId || session.id,
    configDir,
    session.nativeHistoryCwd || session.cwd,
  );
  const snapshots = new Map<string, Record<string, unknown>>(),
    deltas = new Map<string, Record<string, unknown>[]>();
  const input = createReadStream(file);
  const lines = createInterface({ input, crlfDelay: Infinity });
  try {
    for await (const line of lines) {
      const record: unknown = JSON.parse(line);
      if (!isRecord(record)) throw invalid();
      if (
        typeof record.type === "string" &&
        record.type.startsWith("file-history-") &&
        !["file-history-snapshot", "file-history-delta"].includes(record.type)
      )
        throw invalid();
      if (record.type === "file-history-snapshot") {
        if (typeof record.messageId !== "string") throw invalid();
        snapshots.set(record.messageId, record);
      }
      if (record.type === "file-history-delta") {
        if (typeof record.messageId !== "string") throw invalid();
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
  const ordered: (Omit<NativeSnapshot, "trackedFileBackups"> & {
      trackedFileBackups: Record<string, unknown>;
    })[] = [],
    positions = new Map<string, number>();
  for (const message of messages) {
    const record = snapshots.get(message.uuid);
    if (record) {
      const snapshot = structuredClone(record.snapshot);
      if (
        !isRecord(snapshot) ||
        typeof snapshot.messageId !== "string" ||
        !snapshot.messageId ||
        !isRecord(snapshot.trackedFileBackups)
      )
        throw invalid();
      const index = record.isSnapshotUpdate
        ? positions.get(snapshot.messageId)
        : undefined;
      if (index === undefined) {
        positions.set(snapshot.messageId, ordered.length);
        ordered.push({
          messageId: snapshot.messageId,
          trackedFileBackups: snapshot.trackedFileBackups,
        });
      } else
        ordered[index] = {
          messageId: snapshot.messageId,
          trackedFileBackups: snapshot.trackedFileBackups,
        };
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
      const snapshot = ordered[index];
      if (!snapshot) throw invalid();
      Object.defineProperty(snapshot.trackedFileBackups, delta.trackingPath, {
        value: delta.backup,
        enumerable: true,
        configurable: true,
        writable: true,
      });
    }
  }
  const retained = ordered.slice(-NATIVE_FILE_SNAPSHOT_COUNT);
  if (!retained.length) throw invalid();
  // Normalize native absolute and relative tracking names before selecting backups.
  for (const snapshot of retained) {
    const normalized: Record<string, unknown> = Object.create(null);
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
        .find((value) => isRecord(value) && value.version === 1);
    if (
      !isRecord(backup) ||
      !(
        backup.backupFileName === null ||
        (typeof backup.backupFileName === "string" &&
          /^[0-9a-f]{16}@v\d+$/.test(backup.backupFileName))
      )
    )
      throw invalid();
    if (backup.realParentDir && typeof backup.realParentDir !== "string")
      throw invalid();
    return {
      file,
      backup: {
        backupFileName: backup.backupFileName,
        realParentDir:
          typeof backup.realParentDir === "string"
            ? backup.realParentDir
            : undefined,
      },
    };
  });
}

// Admission limits preserve all referenced recovery copies. They never expire.
// This module owns the total bytes and directory count per bridge state root.
export const MAX_BEFORE_RESTORE_BYTES = 1024 ** 3;
export const MAX_BEFORE_RESTORE_COPIES = 100;
const preparations = new Map<string, Promise<unknown>>();
async function withBackupOwner<T>(
  stateDir: string,
  action: () => Promise<T>,
): Promise<T> {
  const key = path.resolve(stateDir);
  const work = (preparations.get(key) || Promise.resolve())
    .catch(() => {})
    .then(action);
  preparations.set(key, work);
  try {
    return await work;
  } finally {
    if (preparations.get(key) === work) preparations.delete(key);
  }
}
async function backupUsage(stateDir: string, skip: string | null = null) {
  const parent = path.join(stateDir, "file-rewinds");
  await fs.mkdir(parent, { recursive: true, mode: 0o700 });
  if ((await fs.realpath(parent)) !== path.resolve(parent)) throw invalid();
  const entries = await fs.readdir(parent, { withFileTypes: true });
  let bytes = 0;
  for (const entry of entries) {
    if (!entry.isDirectory() || entry.isSymbolicLink()) throw invalid();
    const directory = path.join(parent, entry.name);
    if (directory === skip) continue;
    for (const name of await fs.readdir(directory)) {
      const stat = await fs.lstat(path.join(directory, name));
      if (!stat.isFile() || stat.isSymbolicLink() || stat.nlink > 1)
        throw invalid();
      bytes += stat.size;
    }
  }
  return {
    count: entries.length,
    bytes,
    maxCount: MAX_BEFORE_RESTORE_COPIES,
    maxBytes: MAX_BEFORE_RESTORE_BYTES,
  };
}
async function admitBackup(
  parent: string,
  current: string,
  requiredBytes: number,
) {
  const usage = await backupUsage(path.dirname(parent), current);
  if (
    usage.count > usage.maxCount ||
    usage.bytes + requiredBytes > usage.maxBytes
  )
    throw new Error(
      "The before-restore backup limit is reached. Release completed backups before another restore.",
    );
}
export function prepareFileRestore(
  session: RestoreSession,
  messages: Pick<SessionMessage, "uuid">[],
  messageId: string,
  stateDir: string,
  configDir?: string,
) {
  return withBackupOwner(stateDir, () =>
    prepareFileRestoreOwned(session, messages, messageId, stateDir, configDir),
  );
}

// Runtime supplies only durable completed operation identities for this chat.
// The bridge also checks every descriptor reference before deleting a directory.
export function fileBackups<S extends Pick<Session, "controlRequests">>(
  session: S,
  settledRequestIds: string[] | null,
  stateDir: string,
  persist: (session: S) => Promise<void>,
) {
  return withBackupOwner(stateDir, async () => {
    const before = await backupUsage(stateDir);
    if (settledRequestIds === null) return { ...before, released: 0 };
    if (
      !Array.isArray(settledRequestIds) ||
      !settledRequestIds.every((id) => typeof id === "string")
    )
      throw invalid();
    const settled = new Set(settledRequestIds);
    const references = new Map<
      string,
      { id: string; receipt: ControlReceipt }[]
    >();
    for (const [id, receipt] of Object.entries(session.controlRequests || {})) {
      const directory = receipt.filePlan?.directory;
      if (!directory) continue;
      const refs = references.get(directory) || [];
      refs.push({ id, receipt });
      references.set(directory, refs);
    }
    let released = 0;
    for (const [directory, refs] of references) {
      if (
        !refs.every(
          ({ id, receipt }) =>
            settled.has(id) &&
            receipt.status === "completed" &&
            receipt.result?.filesRestored === true,
        )
      )
        continue;
      const parent = path.resolve(stateDir, "file-rewinds");
      if (
        path.dirname(directory) !== parent ||
        !/^before-[A-Za-z0-9]+$/.test(path.basename(directory))
      )
        throw invalid();
      try {
        const stat = await fs.lstat(directory);
        if (!stat.isDirectory() || stat.isSymbolicLink()) throw invalid();
        if (
          !refs.every(
            ({ receipt }) => receipt.fileBackupReleaseAuthorized === true,
          )
        ) {
          const manifest = path.join(directory, "manifest.json");
          if (!(await identity(manifest))) throw invalid();
          const plan = parseBackupManifest(await fs.readFile(manifest, "utf8"));
          if (!refs.every(({ receipt }) => same(receipt.filePlan, plan)))
            throw invalid();
          for (const { receipt } of refs)
            receipt.fileBackupReleaseAuthorized = true;
          await persist(session);
        }
        await fs.rm(directory, { recursive: true });
        released++;
      } catch (error) {
        if (!isRecord(error) || error.code !== "ENOENT") throw error;
      }
      for (const { receipt } of refs) receipt.fileBackupReleased = true;
    }
    await persist(session);
    return { ...(await backupUsage(stateDir)), released };
  });
}

async function prepareFileRestoreOwned(
  session: RestoreSession,
  messages: Pick<SessionMessage, "uuid">[],
  messageId: string,
  stateDir: string,
  configDir = process.env.CLAUDE_CONFIG_DIR ||
    path.join(os.homedir(), ".claude"),
) {
  const cwd = await fs.realpath(session.cwd);
  const entries = await nativeBackups(session, messages, messageId, configDir);
  const files: FileRestorePlan["files"] = [];
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
  try {
    for (const [i, entry] of files.entries())
      if (entry.before) entry.backup = path.join(directory, String(i));
    const plan = { cwd, directory, files };
    const manifestText = JSON.stringify(plan);
    await admitBackup(
      parent,
      directory,
      files.reduce(
        (sum, entry) => sum + (entry.before?.bytes || 0),
        Buffer.byteLength(manifestText),
      ),
    );
    for (const entry of files) {
      if (entry.before) {
        if (entry.backup === undefined) throw invalid();
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
    const manifest = await fs.open(
      path.join(directory, "manifest.json"),
      "wx",
      0o600,
    );
    try {
      await manifest.writeFile(manifestText);
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
    // A returned plan can already be referenced by a durable or uncertain receipt.
    // Retain these recovery copies with the session; never expire them by age.
    return plan;
  } catch (error) {
    // No plan reached the caller, so no file effect or receipt can depend on it.
    try {
      await fs.rm(directory, { recursive: true, force: true });
    } catch (cleanup) {
      throw new AggregateError(
        [error, cleanup],
        "File restore preparation and backup cleanup failed",
      );
    }
    throw error;
  }
}
export async function verifyFileRestore(
  plan: FileRestorePlan,
  stage: "before" | "expected",
) {
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
