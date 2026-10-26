import { nativeInputIds } from "./input-messages.ts";
import { nativeUuid } from "./native-history.mjs";
import { readHistoryIndex } from "./history-index.mjs";

// A native user UUID binds the mirror turn to one transcript chain. Text and
// timestamps alone cannot distinguish an earlier reply or a different branch.
function outcome(session, turn, records) {
  const initial = turn.items.find((item) => item.type === "userMessage");
  const userId = nativeInputIds(initial)?.[0];
  if (!nativeUuid(userId) || !records) return null;
  const rows = records.map(({ row }) => row);
  const anchors = rows.filter((row) => row.uuid === userId);
  if (anchors.length !== 1) return null;
  const anchor = anchors[0];
  if (
    anchor.type !== "user" ||
    anchor.isSidechain !== false ||
    anchor.isCompactSummary
  )
    return null;
  const seen = new Set();
  const counts = new Map();
  for (const row of rows)
    if (row.uuid) counts.set(row.uuid, (counts.get(row.uuid) || 0) + 1);
  const pending = new Set();
  let previous,
    last,
    responseId,
    texts = [];
  for (const row of rows.slice(rows.indexOf(anchor))) {
    if (row.isSidechain === true) continue;
    if (
      row.isCompactSummary ||
      row.type === "summary" ||
      row.subtype === "compact_boundary"
    )
      return null;
    if (
      [
        "system",
        "progress",
        "file-history-snapshot",
        "queue-operation",
        "last-prompt",
      ].includes(row.type)
    )
      continue;
    if (
      !["assistant", "user"].includes(row.type) ||
      !nativeUuid(row.uuid) ||
      seen.has(row.uuid) ||
      counts.get(row.uuid) !== 1 ||
      row.sessionId !== (session.nativeId || session.id) ||
      row.cwd !== (session.nativeHistoryCwd || session.cwd) ||
      row.isSidechain !== false ||
      (row.isMeta &&
        !turn.items.some(
          (item) =>
            item.type === "userMessage" &&
            nativeInputIds(item)?.includes(row.uuid),
        )) ||
      row.isVisibleInTranscriptOnly ||
      (previous && row.parentUuid !== previous)
    )
      return null;
    seen.add(row.uuid);
    previous = row.uuid;
    last = row;
    if (row === anchor) {
      continue;
    }
    const blocks = row.message?.content;
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
          nativeInputIds(item)?.includes(row.uuid),
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
      if (row.isApiErrorMessage || typeof row.message.id !== "string")
        return null;
      if (responseId !== row.message.id) texts = [];
      responseId = row.message.id;
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
      last.message.content.some((block) => block.type === "tool_use"))
  )
    return { status: "interrupted" };
  // Tool receipts can deliberately end a turn (for example a permission denial).
  // Without the native tool policy, that tail does not authorize another turn.
  if (last.type !== "assistant" || pending.size) return null;
  // Claude's resume classifier treats a final assistant reply without unresolved
  // calls as ended. Null stop_reason occurs in native streamed transcript rows.
  if (
    ![undefined, null, "end_turn", "stop_sequence"].includes(
      last.message.stop_reason,
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

export async function reconcileRestartTurns(session, options) {
  const stale = session.turns.filter((turn) => turn.status === "inProgress");
  if (!stale.length) return;
  const index = await readHistoryIndex(session, options);
  const records = index?.entries.map((row) => ({ row }));
  for (const turn of stale) {
    const result = outcome(session, turn, records);
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
        ["preparing", "submitted"].includes(item.deliveryStatus)
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
