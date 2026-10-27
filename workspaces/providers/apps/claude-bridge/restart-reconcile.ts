import type { Session, Turn, Item } from "./session-state.ts";
import type { SessionStoreEntry } from "@anthropic-ai/claude-agent-sdk";
import type {
  HistoryReadOptions,
  VerifiedHistoryRecord,
} from "./history-index.ts";
import { nativeInputIds } from "./input-messages.ts";
import { nativeUuid } from "./native-history.ts";
import { readHistoryIndex } from "./history-index.ts";

// A native user UUID binds the mirror turn to one transcript chain. Text and
// timestamps alone cannot distinguish an earlier reply or a different branch.
function outcome(
  turn: Turn,
  records: readonly VerifiedHistoryRecord[] | undefined,
): { status: "interrupted" } | { status: "completed"; item: Item } | null {
  const initial = turn.items.find((item) => item.type === "userMessage");
  const userId = nativeInputIds(initial)?.[0];
  if (!nativeUuid(userId) || !records) return null;
  const start = records.findIndex(({ row }) => row.uuid === userId);
  const anchor = records[start]?.row;
  if (
    !anchor ||
    anchor.type !== "user" ||
    anchor.isSidechain !== false ||
    anchor.isCompactSummary
  )
    return null;
  const pending = new Set<unknown>();
  let previous: string | undefined,
    last: SessionStoreEntry | undefined,
    responseId: string | undefined;
  let texts: string[] = [];
  for (const { row, kind, parentMessageId } of records.slice(start)) {
    if (row.isSidechain === true) continue;
    if (
      row.isCompactSummary ||
      row.type === "summary" ||
      row.subtype === "compact_boundary"
    )
      return null;
    if (kind === "metadata" || kind === "queue") continue;
    if (row.type !== "assistant" && row.type !== "user") {
      // An opaque record may be input that changed this turn's answer.
      if (kind === "input" || kind === "opaque") return null;
      continue;
    }
    if (
      (row.isMeta &&
        !turn.items.some(
          (item) =>
            item.type === "userMessage" &&
            nativeInputIds(item)?.includes(row.uuid!),
        )) ||
      row.isVisibleInTranscriptOnly ||
      (previous && parentMessageId !== previous)
    )
      return null;
    previous = row.uuid;
    last = row;
    if (row === anchor) {
      continue;
    }
    const blocks = (row.message as Record<string, unknown>)?.content as
      | Record<string, unknown>[]
      | undefined;
    if (
      !Array.isArray(blocks) ||
      blocks.some((block) => !block || typeof block.type !== "string")
    )
      return null;
    if (row.type === "user") {
      // A later input must belong to this turn (tool receipts or saved steers).
      const steer = turn.items.some(
        (item) =>
          item.type === "userMessage" &&
          nativeInputIds(item)?.includes(row.uuid!),
      );
      if (
        !steer &&
        (!blocks.length || blocks.some((block) => block.type !== "tool_result"))
      )
        return null;
      for (const block of blocks) {
        if (block.type === "tool_result" && !pending.delete(block.tool_use_id))
          return null;
      }
      texts = [];
      responseId = undefined;
    } else {
      if (
        row.isApiErrorMessage ||
        typeof (row.message as Record<string, unknown>).id !== "string"
      )
        return null;
      if (responseId !== (row.message as Record<string, unknown>).id)
        texts = [];
      responseId = (row.message as Record<string, unknown>).id as string;
      for (const block of blocks) {
        if (block.type === "text" && typeof block.text === "string")
          texts.push(block.text);
        if (block.type === "tool_use") {
          if (typeof block.id !== "string" || pending.has(block.id))
            return null;
          pending.add(block.id);
        }
      }
    }
  }
  if (!last) return null;
  if (
    last === anchor ||
    (last.type === "assistant" &&
      pending.size &&
      (
        (last.message as Record<string, unknown>).content as Record<
          string,
          unknown
        >[]
      ).some((block) => block.type === "tool_use"))
  )
    return { status: "interrupted" };
  // Tool receipts can deliberately end a turn (for example a permission denial).
  // Without the native tool policy, that tail does not authorize another turn.
  if (last.type !== "assistant" || pending.size) return null;
  // Claude's resume classifier treats a final assistant reply without unresolved
  // calls as ended. Null stop_reason occurs in native streamed transcript rows.
  if (
    ![undefined, null, "end_turn", "stop_sequence"].includes(
      (last.message as Record<string, unknown>).stop_reason as
        | string
        | null
        | undefined,
    ) ||
    !texts.join("\n")
  )
    return null;
  return {
    status: "completed",
    item: {
      id: responseId,
      nativeId: last.uuid,
      type: "agentMessage",
      text: texts.join("\n"),
      phase: "final_answer",
    },
  };
}

export async function reconcileRestartTurns(
  session: Session,
  options?: HistoryReadOptions,
) {
  const stale = session.turns.filter((turn) => turn.status === "inProgress");
  if (!stale.length) return;
  const index = await readHistoryIndex(session, options);
  const records = index?.records;
  for (const turn of stale) {
    const result = outcome(turn, records);
    const initial = turn.items.find((item) => item.type === "userMessage");
    for (const [itemIndex, item] of turn.items.entries()) {
      if (item.type !== "userMessage") continue;
      if (
        index?.outcome(nativeInputIds(item), nativeInputIds(initial)) !==
        "delivered"
      )
        continue;
      if (itemIndex === 0 && turn.startOutcome === "preparing")
        turn.startOutcome = "accepted";
      if (
        item.delivery === "steer" &&
        ["preparing", "submitted"].includes(item.deliveryStatus!)
      ) {
        item.deliveryStatus = "delivered";
        delete item.error;
      }
    }
    turn.status = result?.status || "uncertain";
    if (result?.status === "completed") {
      const existing = turn.items.findIndex(
        (item) => item.id === result.item.id,
      );
      if (existing < 0) turn.items.push(result.item);
      else turn.items[existing] = { ...turn.items[existing], ...result.item };
      delete turn.error;
    } else {
      turn.error = {
        message: result
          ? "The native Claude transcript confirms an incomplete turn."
          : "Studio could not confirm the Claude turn outcome. Review the native transcript before continuing.",
      };
    }
  }
}
