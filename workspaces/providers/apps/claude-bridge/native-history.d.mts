import type { FileHandle } from "node:fs/promises";
import type { Stats } from "node:fs";
import type { SessionStoreEntry } from "@anthropic-ai/claude-agent-sdk";
export const NATIVE_HISTORY_TAIL_BYTES: number;
export function nativeUuid(value: unknown): value is string;
export interface NativeTail {
  offset: number;
  raw: Buffer;
  before: Stats;
  handle: FileHandle;
}
export function nativeHistoryRows(
  tail: NativeTail,
): Promise<{ row: SessionStoreEntry; offset: number }[]>;
export function readNativeHistory<
  T = { row: SessionStoreEntry; offset: number }[],
>(
  session: {
    id: string;
    nativeId?: string;
    cwd: string;
    nativeHistoryCwd?: string;
  },
  options?: object,
  inspect?: (tail: NativeTail) => Promise<T | null>,
): Promise<T | null>;
