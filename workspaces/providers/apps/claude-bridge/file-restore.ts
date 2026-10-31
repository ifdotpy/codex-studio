import type {
  Session,
  FileRestorePlan,
  ControlReceipt,
} from "./session-state.ts";
// Release of before-restore backup copies written by earlier restores.
import fs from "node:fs/promises";
import { constants } from "node:fs";
import { createHash } from "node:crypto";
import path from "node:path";

const invalid = () =>
  new Error(
    "The SDK file checkpoint cannot be verified. Inspect workspace recovery.",
  );

async function identity(file: string) {
  let stat;
  try {
    stat = await fs.lstat(file);
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") return null;
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
// This manifest is written by earlier restores in this bridge and compared
// byte-for-byte as JSON against the durable receipt before any deletion.
function parseBackupManifest(text: string): FileRestorePlan {
  return JSON.parse(text);
}
function same(a: unknown, b: unknown) {
  return JSON.stringify(a) === JSON.stringify(b);
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
        if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error;
      }
      for (const { receipt } of refs) receipt.fileBackupReleased = true;
    }
    await persist(session);
    return { ...(await backupUsage(stateDir)), released };
  });
}
