import type { FileHandle, FileReadResult } from "node:fs/promises";
import type { Stats } from "node:fs";
import fs from "node:fs/promises";
import { constants } from "node:fs";
import os from "node:os";
import path from "node:path";

type NativeStat = Pick<
  Stats,
  "size" | "dev" | "ino" | "mtimeMs" | "isFile" | "isSymbolicLink"
>;
export type NativeHistoryHandle = Pick<FileHandle, "close"> & {
  stat: () => Promise<NativeStat>;
  read: (
    buffer: Buffer,
    offset: number,
    length: number,
    position: number,
  ) => Promise<Pick<FileReadResult<Buffer>, "bytesRead">>;
};
export type NativeHistoryFilesystem = {
  realpath: (file: string) => Promise<string>;
  open: (file: string, flags: number) => Promise<NativeHistoryHandle>;
  lstat: (file: string) => Promise<NativeStat>;
};
export interface NativeTail {
  offset: number;
  raw: Buffer;
  before: NativeStat;
  handle: NativeHistoryHandle;
}
export interface HistorySession {
  id: string;
  nativeId?: string | undefined;
  cwd: string;
  nativeHistoryCwd?: string | undefined;
}
export interface NativeHistoryOptions {
  configDir?: string;
  filesystem?: NativeHistoryFilesystem;
}
export interface NativeHistoryRow {
  row: Record<string, unknown>;
  offset: number;
}

// Maximum bytes read from one native transcript during recovery.
export const NATIVE_HISTORY_TAIL_BYTES = 8 * 1024 * 1024;
export const nativeUuid = (value: unknown): value is string =>
  typeof value === "string" &&
  /^[a-f0-9]{8}-(?:[a-f0-9]{4}-){3}[a-f0-9]{12}$/i.test(value);

async function readTail(
  file: string,
  filesystem: NonNullable<NativeHistoryOptions["filesystem"]>,
) {
  // Resolve the account's own native path. Never follow a transcript symlink.
  if ((await filesystem.realpath(file)) !== file) return null;
  const handle = await filesystem.open(
    file,
    constants.O_RDONLY | constants.O_NOFOLLOW,
  );
  let retained = false;
  try {
    const before = await handle.stat();
    if (!before.isFile() || !Number.isSafeInteger(before.size)) return null;
    const offset = Math.max(0, before.size - NATIVE_HISTORY_TAIL_BYTES);
    const raw = Buffer.alloc(before.size - offset);
    const read = await handle.read(raw, 0, raw.length, offset);
    if (read.bytesRead !== raw.length || raw.at(-1) !== 10) return null;
    retained = true;
    return { handle, before, offset, raw };
  } finally {
    if (!retained) await handle.close();
  }
}

// Return complete bounded records. A torn native tail cannot prove an outcome.
export async function nativeHistoryRows(
  tail: NativeTail,
): Promise<NativeHistoryRow[]> {
  const rows = [];
  let start = tail.offset ? tail.raw.indexOf(10) + 1 : 0;
  let yielded = start;
  while (start < tail.raw.length) {
    const end = tail.raw.indexOf(10, start);
    if (end < 0 || end - start > NATIVE_HISTORY_TAIL_BYTES)
      throw new Error("Invalid native history record");
    const row: unknown = JSON.parse(
      tail.raw.subarray(start, end).toString("utf8"),
    );
    if (!row || typeof row !== "object" || Array.isArray(row))
      throw new Error("Invalid native history row");
    rows.push({ row: { ...row }, offset: start });
    start = end + 1;
    if (start - yielded >= 256 * 1024) {
      await new Promise<void>((resolve) => setImmediate(resolve));
      yielded = start;
    }
  }
  return rows;
}

export function readNativeHistory(
  session: HistorySession,
  options?: NativeHistoryOptions,
): Promise<NativeHistoryRow[] | null>;
export function readNativeHistory<T>(
  session: HistorySession,
  options: NativeHistoryOptions | undefined,
  inspect: (tail: NativeTail) => Promise<T[] | null>,
): Promise<T[] | null>;
export async function readNativeHistory<T>(
  session: HistorySession,
  {
    configDir = process.env.CLAUDE_CONFIG_DIR ||
      path.join(os.homedir(), ".claude"),
    filesystem = fs,
  }: NativeHistoryOptions = {},
  inspect:
    | ((tail: NativeTail) => Promise<T[] | null>)
    | typeof nativeHistoryRows = nativeHistoryRows,
) {
  if (
    !nativeUuid(session.nativeId || session.id) ||
    !path.isAbsolute(session.cwd || "")
  )
    return null;
  const cwd = session.nativeHistoryCwd || session.cwd;
  if (typeof cwd !== "string" || !path.isAbsolute(cwd)) return null;
  // Claude hashes longer project directory names. Leave that unsupported scope
  // unknown rather than searching other projects or guessing its native path.
  const project = cwd.replace(/[^a-zA-Z0-9]/g, "-");
  if (project.length > 200) return null;
  const file = path.join(
    path.resolve(configDir),
    "projects",
    project,
    (session.nativeId || session.id) + ".jsonl",
  );
  let tail;
  try {
    tail = await readTail(file, filesystem);
    if (!tail) return null;
    const found = await inspect(tail);
    if (!found?.length) return found;
    const current = await filesystem.lstat(file);
    const after = await tail.handle.stat();
    if (
      current.isSymbolicLink() ||
      current.dev !== tail.before.dev ||
      current.ino !== tail.before.ino ||
      after.size !== tail.before.size ||
      after.mtimeMs !== tail.before.mtimeMs
    )
      return null;
    const pinned = Buffer.alloc(tail.raw.length);
    const read = await tail.handle.read(pinned, 0, pinned.length, tail.offset);
    if (read.bytesRead !== pinned.length || !pinned.equals(tail.raw))
      return null;
    const finalPath = await filesystem.lstat(file),
      finalFile = await tail.handle.stat();
    if (
      finalPath.dev !== tail.before.dev ||
      finalPath.ino !== tail.before.ino ||
      finalFile.size !== tail.before.size ||
      finalFile.mtimeMs !== tail.before.mtimeMs
    )
      return null;
    return found;
  } catch {
    return null;
  } finally {
    await tail?.handle.close();
  }
}
