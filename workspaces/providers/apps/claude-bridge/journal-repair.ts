import type { Session } from "./session-state.ts";
import type { NativeHistoryOptions } from "./native-history.ts";
import {
  readNativeHistory,
  nativeHistoryRows,
  nativeUuid,
} from "./native-history.ts";

export interface JournalChange {
  op: "set" | "delete" | "append";
  path: (string | number)[];
  value?: unknown;
}
export interface JournalRecord {
  sequence: number;
  changes: JournalChange[];
}
export function validJournalRecord(value: unknown): value is JournalRecord {
  // The original checks below define the untrusted journal-record boundary.
  const record = value as JournalRecord;
  return (
    record &&
    Number.isSafeInteger(record.sequence) &&
    record.sequence > 0 &&
    Array.isArray(record.changes) &&
    record.changes.every(
      (change) =>
        change &&
        ["set", "delete", "append"].includes(change.op) &&
        Array.isArray(change.path) &&
        change.path.every(
          (key: unknown) =>
            (typeof key === "string" ||
              (Number.isSafeInteger(key) && (key as number) >= 0)) &&
            !["__proto__", "prototype", "constructor"].includes(key as string),
        ) &&
        (change.op === "delete" || Object.hasOwn(change, "value")) &&
        (change.op !== "append" ||
          (change.path.length > 0 && Array.isArray(change.value))) &&
        (change.op !== "delete" || change.path.length > 0),
    )
  );
}

function textContent(content: unknown) {
  if (typeof content === "string") return content;
  if (
    !Array.isArray(content) ||
    content.some(
      (block: unknown) =>
        (block as { type?: unknown } | null)?.type !== "text" ||
        typeof (block as { text?: unknown }).text !== "string",
    )
  )
    return null;
  return content.map((block: { text: string }) => block.text).join("\n");
}

// Repair requires complete, exact native message coverage. Tool receipts,
// compaction and unresolved control operations need a separate manual repair.
export async function nativeConfirmsJournal(
  session: Session,
  options?: NativeHistoryOptions,
) {
  if (
    !Array.isArray(session.turns) ||
    !session.turns.length ||
    Object.values(session.controlRequests || {}).some(
      (receipt) => receipt.status !== "completed",
    )
  )
    return false;
  const expected: { type: string; uuid: string; text: string }[] = [];
  for (const turn of session.turns) {
    if (
      turn.status !== "completed" ||
      !Array.isArray(turn.items) ||
      !turn.items.length
    )
      return false;
    for (const item of turn.items) {
      if (!nativeUuid(item.nativeId) || item.thinking) return false;
      if (item.type === "userMessage") {
        const text = textContent(item.content);
        if (text === null) return false;
        expected.push({ type: "user", uuid: item.nativeId, text });
      } else if (
        item.type === "agentMessage" &&
        typeof item.text === "string"
      ) {
        expected.push({
          type: "assistant",
          uuid: item.nativeId,
          text: item.text,
        });
      } else return false;
    }
    if (expected.at(-1)?.type !== "assistant") return false;
  }
  let cwd = "";
  const records = await readNativeHistory(session, options, (tail) => {
    cwd = tail.cwd;
    return nativeHistoryRows(tail);
  });
  if (!records) return false;
  const messages: { type: unknown; uuid: unknown; text: string }[] = [];
  for (const { row } of records) {
    if (row.isSidechain === true) continue;
    if (
      row.isCompactSummary ||
      row.type === "summary" ||
      row.subtype === "compact_boundary"
    )
      return false;
    if (
      [
        "system",
        "progress",
        "file-history-snapshot",
        "queue-operation",
        "last-prompt",
      ].includes(row.type as string)
    )
      continue;
    if (
      !["user", "assistant"].includes(row.type as string) ||
      row.isSidechain !== false ||
      row.isMeta ||
      row.isApiErrorMessage ||
      row.isVisibleInTranscriptOnly ||
      row.sessionId !== (session.nativeId || session.id) ||
      row.cwd !== cwd ||
      row.parentUuid !== (messages.at(-1)?.uuid ?? null)
    )
      return false;
    if (
      row.type === "assistant" &&
      ![undefined, null, "end_turn", "stop_sequence"].includes(
        (row.message as { stop_reason?: string | null })?.stop_reason,
      )
    )
      return false;
    const text = textContent((row.message as { content?: unknown })?.content);
    if (text === null) return false;
    messages.push({ type: row.type, uuid: row.uuid, text });
  }
  return JSON.stringify(messages) === JSON.stringify(expected);
}
