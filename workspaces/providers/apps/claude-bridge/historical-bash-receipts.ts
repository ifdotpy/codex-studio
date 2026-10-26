import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { isDeepStrictEqual } from "node:util";

import {
  readNativeHistory,
  nativeHistoryRows,
  NATIVE_HISTORY_TAIL_BYTES,
  nativeUuid as uuid,
} from "./native-history.mjs";
export const HISTORICAL_BASH_TAIL_BYTES = NATIVE_HISTORY_TAIL_BYTES;
const MAX_ITEMS = 64;
const object = (value) =>
  value !== null && typeof value === "object" && !Array.isArray(value);

function candidates(session) {
  const found = [];
  const ids = new Set();
  for (const turn of session.turns || []) {
    for (const item of turn.items || []) {
      if (
        item.type !== "commandExecution" ||
        item.tool !== "Bash" ||
        item.status !== "inProgress"
      )
        continue;
      if (
        !turn.id ||
        typeof item.id !== "string" ||
        !item.id ||
        ids.has(item.id) ||
        !object(item.arguments) ||
        typeof item.arguments.command !== "string" ||
        item.command !== item.arguments.command ||
        item.cwd !== session.cwd ||
        ![undefined, false].includes(item.arguments.run_in_background)
      )
        return [];
      ids.add(item.id);
      found.push({
        turn,
        item,
        saved: JSON.stringify(item),
        status: turn.status,
      });
      if (found.length > MAX_ITEMS) return [];
    }
  }
  return found;
}

async function receipts(tail, selected, session) {
  const ids = new Set(selected.map(({ item }) => item.id));
  const uses = new Map(),
    results = new Map(),
    uuids = new Set();
  for (const { row, offset: start } of await nativeHistoryRows(tail)) {
    if (
      !object(row) ||
      row.isCompactSummary === true ||
      row.type === "summary" ||
      (row.type === "system" && row.subtype === "compact_boundary")
    )
      return [];
    if (Array.isArray(row.message?.content)) {
      const blocks = row.message.content.filter(
        (block) =>
          object(block) &&
          ((block.type === "tool_use" && ids.has(block.id)) ||
            (block.type === "tool_result" && ids.has(block.tool_use_id))),
      );
      if (blocks.length) {
        if (!uuid(row.uuid) || uuids.has(row.uuid)) return [];
        uuids.add(row.uuid);
      }
      for (const block of blocks) {
        const id = block.type === "tool_use" ? block.id : block.tool_use_id;
        const map = block.type === "tool_use" ? uses : results;
        if (map.has(id)) return [];
        map.set(id, { row, block, offset: start });
      }
    }
  }

  const found = [];
  for (const entry of selected) {
    const use = uses.get(entry.item.id),
      result = results.get(entry.item.id);
    if (!use || !result) continue;
    const outcome = result.row.toolUseResult;
    const startedAtMs = Date.parse(use.row.timestamp),
      completedAtMs = Date.parse(result.row.timestamp);
    if (
      use.row.type !== "assistant" ||
      result.row.type !== "user" ||
      result.offset <= use.offset ||
      [use.row, result.row].some(
        (row) =>
          row.sessionId !== (session.nativeId || session.id) ||
          row.isSidechain !== false ||
          row.cwd !== entry.item.cwd ||
          row.isMeta === true ||
          row.isVisibleInTranscriptOnly === true,
      ) ||
      result.row.parentUuid !== use.row.uuid ||
      result.row.sourceToolAssistantUUID !== use.row.uuid ||
      use.block.name !== "Bash" ||
      !isDeepStrictEqual(use.block.input, entry.item.arguments) ||
      result.block.type !== "tool_result" ||
      result.block.is_error !== false ||
      !object(outcome) ||
      outcome.interrupted !== false ||
      outcome.isImage === true ||
      outcome.noOutputExpected === true ||
      Object.keys(outcome).some(
        (key) =>
          ![
            "stdout",
            "stderr",
            "interrupted",
            "isImage",
            "noOutputExpected",
          ].includes(key),
      ) ||
      typeof outcome.stdout !== "string" ||
      typeof outcome.stderr !== "string" ||
      result.block.content !== outcome.stdout + outcome.stderr ||
      typeof use.row.timestamp !== "string" ||
      typeof result.row.timestamp !== "string" ||
      !Number.isFinite(startedAtMs) ||
      !Number.isFinite(completedAtMs) ||
      completedAtMs < startedAtMs
    )
      continue;
    found.push({
      ...entry,
      completed: {
        ...entry.item,
        status: "completed",
        exitCode: 0,
        aggregatedOutput: result.block.content,
        result: { content: result.block.content },
      },
      times: { startedAtMs, completedAtMs },
    });
  }
  return found;
}

// Only resume consumes positive old foreground receipts. Status polls never read
// native history. Missing, compacted or older-than-budget results stay unknown.
export async function reconcileHistoricalBash(
  session,
  {
    isIdle,
    complete,
    persist,
    configDir = process.env.CLAUDE_CONFIG_DIR ||
      path.join(os.homedir(), ".claude"),
    filesystem = fs,
  },
) {
  if (!isIdle() || !uuid(session.nativeId || session.id)) return 0;
  const selected = candidates(session);
  if (!selected.length || !path.isAbsolute(session.cwd || "")) return 0;
  const source = JSON.stringify([
    session.id,
    session.nativeId,
    session.cwd,
    session.nativeHistoryCwd,
  ]);
  const turns = session.turns,
    turnCount = turns.length;
  const found = await readNativeHistory(
    session,
    { configDir, filesystem },
    (tail) => receipts(tail, selected, session),
  );
  if (!found?.length) return 0;

  if (
    !isIdle() ||
    session.turns !== turns ||
    turns.length !== turnCount ||
    source !==
      JSON.stringify([
        session.id,
        session.nativeId,
        session.cwd,
        session.nativeHistoryCwd,
      ]) ||
    selected.some(
      ({ turn, item, saved, status }) =>
        !turns.includes(turn) ||
        turn.status !== status ||
        !turn.items.includes(item) ||
        JSON.stringify(item) !== saved,
    )
  )
    return 0;
  for (const { turn, completed, times } of found)
    complete(turn, completed, times);
  await persist(session);
  return found.length;
}
