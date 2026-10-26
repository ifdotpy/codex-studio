import type { HistorySession, NativeHistoryOptions } from "./native-history.ts";
export interface HistoryReadOptions extends NativeHistoryOptions {
  sessionApi?: Pick<
    typeof sdk,
    "getSessionInfo" | "getSessionMessages" | "importSessionToStore"
  >;
}
interface ChainEntry extends sdk.SessionStoreEntry {
  uuid: string;
  type: "user" | "assistant";
}
import * as sdk from "@anthropic-ai/claude-agent-sdk";
import { historyRecordKind, object } from "./history-records.ts";
import { isDeepStrictEqual } from "node:util";
import {
  readNativeHistory,
  nativeHistoryRows,
  nativeUuid,
} from "./native-history.ts";

/** One UUID lookup for a complete history snapshot, shared by controls and recovery.
 * @param {sdk.SessionMessage[]} messages
 * @param {Set<string>} [inputIds]
 * @param {Set<string>} [uncertainIds]
 */
export function messageIndex(
  messages: sdk.SessionMessage[],
  inputIds: Set<string> = new Set(
    messages
      .filter((message) => message.type === "user")
      .map((message) => message.uuid),
  ),
  uncertainIds: Set<string> = new Set(),
) {
  /** @type {Map<string, number>} */
  const positions: Map<string, number> = new Map();
  for (const [position, message] of messages.entries())
    positions.set(message.uuid, positions.has(message.uuid) ? -1 : position);
  return {
    messages,
    /** @param {string[]} ids */
    positions(ids: string[]) {
      const found = ids.map((id) => positions.get(id) ?? -1);
      return found.every(
        (position, i) =>
          position >= 0 && (i === 0 || position > (found[i - 1] ?? -1)),
      )
        ? found
        : null;
    },
    /** @param {string[] | null} ids @param {string[] | null} anchors */
    outcome(ids: string[] | null, anchors: string[] | null) {
      if (
        !ids?.length ||
        !anchors?.length ||
        !ids.every(nativeUuid) ||
        !anchors.every(nativeUuid) ||
        new Set(ids).size !== ids.length ||
        new Set(anchors).size !== anchors.length ||
        !this.positions(anchors) ||
        !this.positions(anchors)?.every((position) =>
          inputIds.has(messages[position]?.uuid ?? ""),
        )
      )
        return "unknown";
      const found = this.positions(ids);
      if (found && ids.every((id) => inputIds.has(id))) return "delivered";
      return ids.every((id) => !inputIds.has(id) && !uncertainIds.has(id))
        ? "not_applied"
        : "unknown";
    },
  };
}

// The SDK deliberately omits hidden meta inputs from getSessionMessages. Its
// typed SessionStore import retains those inputs without changing their prompt.
// The raw reader is only an integrity guard: SDK parsers silently discard bad
// records. A stable, complete, strictly recognized file is required before any
// absence or branch can be trusted. Unknown CLI formats fail closed.
/** @param {sdk.SessionStoreEntry[]} rows @param {string} sessionId @param {string} cwd */
function verifiedChain(
  rows: sdk.SessionStoreEntry[],
  sessionId: string,
  cwd: string,
) {
  /** @type {sdk.SessionStoreEntry[]} */
  const chain: ChainEntry[] = [];
  const seen = new Set();
  /** @type {Map<string, string | null>} */
  const nearestInput: Map<string, string | null> = new Map();
  /** @type {string | null} */
  let previousInput: string | null = null;
  const inputIds = new Set<string>();
  const uncertainIds = new Set<string>();
  for (const row of rows) {
    if (
      !row ||
      typeof row !== "object" ||
      Array.isArray(row) ||
      typeof row.type !== "string"
    )
      return null;
    const kind = historyRecordKind(row);
    if (!kind) return null;
    if (row.uuid !== undefined) {
      if (!nativeUuid(row.uuid) || seen.has(row.uuid)) return null;
      seen.add(row.uuid);
    }
    if (kind === "queue") {
      if (row.sessionId !== sessionId) return null;
      if (nativeUuid(row.commandUuid)) uncertainIds.add(row.commandUuid);
      continue;
    }
    if (row.isSidechain === true || kind === "metadata") continue;
    if (
      !nativeUuid(row.uuid) ||
      row.sessionId !== sessionId ||
      row.cwd !== cwd ||
      row.isSidechain !== false
    )
      return null;
    if (
      row.parentUuid !== null &&
      (typeof row.parentUuid !== "string" || !nearestInput.has(row.parentUuid))
    )
      return null;
    const ancestor =
      row.parentUuid === null
        ? null
        : (nearestInput.get(row.parentUuid) ?? null);
    // Inputs must belong to one proven conversation; output/progress may be
    // siblings and carry evolving payloads without changing input coverage.
    if (
      kind === "input" &&
      previousInput !== null &&
      ancestor !== previousInput
    )
      return null;
    nearestInput.set(row.uuid, kind === "input" ? row.uuid : ancestor);
    if (kind === "input") previousInput = row.uuid;
    if (row.type === "user" || row.type === "assistant") {
      chain.push({ ...row, type: row.type, uuid: row.uuid });
      if (kind === "input") inputIds.add(row.uuid);
    }
    if (row.type === "attachment" && kind === "input") {
      const attachment = row.attachment;
      if (!object(attachment) || !nativeUuid(attachment.source_uuid))
        return null;
      inputIds.add(attachment.source_uuid);
      chain.push({
        ...row,
        queuedProvenance: true,
        type: "user",
        uuid: attachment.source_uuid,
        message: { role: "user", content: [] },
      });
    }
  }
  return chain.length ? Object.assign(chain, { inputIds, uncertainIds }) : null;
}

/** @param {HistorySession} session @param {HistoryReadOptions} [options] */
export async function readHistoryIndex(
  session: HistorySession,
  options: HistoryReadOptions = {},
) {
  const api = options.sessionApi || sdk;
  const sessionId = session.nativeId || session.id;
  const dir = session.nativeHistoryCwd || session.cwd;
  if (!nativeUuid(sessionId)) return null;
  try {
    const result = await readNativeHistory(session, options, async (tail) => {
      if (tail.offset !== 0) return null;
      const guarded = (await nativeHistoryRows(tail)).map(({ row }) => row);
      const before = await api.getSessionInfo(sessionId, { dir });
      if (
        !before ||
        before.sessionId !== sessionId ||
        before.cwd !== dir ||
        before.fileSize !== tail.before.size
      )
        return null;
      /** @type {sdk.SessionStoreEntry[]} */
      const entries: sdk.SessionStoreEntry[] = [];
      /** @type {sdk.SessionStore} */
      const store: sdk.SessionStore = {
        append: async (key, batch) => {
          if (
            key.sessionId !== sessionId ||
            key.subpath !== undefined ||
            key.projectKey !== dir.replace(/[^a-zA-Z0-9]/g, "-") ||
            !Array.isArray(batch)
          )
            throw new Error("Unexpected Claude history scope");
          entries.push(...batch);
        },
        load: async () => null,
      };
      const imported = await api.importSessionToStore(sessionId, store, {
        dir,
        includeSubagents: false,
      });
      if (imported !== undefined) return null;
      if (!isDeepStrictEqual(entries, guarded)) return null;
      const chain = verifiedChain(entries, sessionId, dir);
      if (!chain) return null;
      const visible = await api.getSessionMessages(sessionId, {
        dir,
        includeSystemMessages: true,
      });
      const byId = new Map(
        entries
          .filter((row) => row.type === "system")
          .concat(chain)
          .map((row) => [row.uuid, row]),
      );
      if (
        !visible.length ||
        visible.some(
          (message) =>
            !nativeUuid(message.uuid) ||
            message.session_id !== sessionId ||
            message.parent_tool_use_id ||
            message.parent_agent_id ||
            !byId.has(message.uuid) ||
            (!byId.get(message.uuid)?.queuedProvenance &&
              !isDeepStrictEqual(
                byId.get(message.uuid)?.message,
                message.message,
              )),
        )
      )
        return null;
      // Every ordinary input/reply must appear in the SDK chain. Meta inputs
      // may be hidden, but must have the exact UUID in the SDK store import.
      const visibleIds = new Set(visible.map((message) => message.uuid));
      const visibleOrder = visible
        .filter((message) => message.type !== "system")
        .map((message) => message.uuid);
      const expectedOrder = chain
        .filter((row) => visibleIds.has(row.uuid))
        .map((row) => row.uuid);
      if (!isDeepStrictEqual(visibleOrder, expectedOrder)) return null;
      if (
        chain.some(
          (row) =>
            chain.inputIds.has(row.uuid) &&
            !row.isMeta &&
            !row.queuedProvenance &&
            !visibleIds.has(row.uuid),
        )
      )
        return null;
      const after = await api.getSessionInfo(sessionId, { dir });
      if (!isDeepStrictEqual(before, after)) return null;
      return [
        Object.assign(
          messageIndex(
            chain.map((row) => ({
              type: row.type,
              uuid: row.uuid,
              session_id: sessionId,
              message: row.message,
              parent_tool_use_id: null,
              parent_agent_id: null,
            })),
            chain.inputIds,
            chain.uncertainIds,
          ),
          { entries },
        ),
      ];
    });
    return result?.[0] ?? null;
  } catch {
    return null;
  }
}

/** Controls cannot perform a native effect from uncertain coverage.
 * @param {HistorySession} session
 */
export async function verifiedHistoryMessages(session: HistorySession) {
  const index = await readHistoryIndex(session);
  if (!index)
    throw new Error(
      "The Claude history is incomplete or uncertain; no native history operation was submitted",
    );
  return index.messages;
}

/** Pure integrity witness for SDK store entries. Production readers also require
 * pinned bytes, SDK metadata, and the typed SDK conversation chain.
 * @param {sdk.SessionStoreEntry[] | undefined | null} rows
 * @param {{sessionId: string, cwd: string, anchorIds: string[] | null, inputIds: string[] | null}} scope
 */
export function inputHistoryOutcome(
  rows: sdk.SessionStoreEntry[] | undefined | null,
  scope: {
    sessionId: string;
    cwd: string;
    anchorIds: string[] | null;
    inputIds: string[] | null;
  },
) {
  if (!Array.isArray(rows)) return "unknown";
  const chain = verifiedChain(rows, scope.sessionId, scope.cwd);
  if (!chain) return "unknown";
  return messageIndex(
    chain.map((row) => ({
      type: row.type,
      uuid: row.uuid,
      session_id: scope.sessionId,
      message: row.message,
      parent_tool_use_id: null,
      parent_agent_id: null,
    })),
    chain.inputIds,
    chain.uncertainIds,
  ).outcome(scope.inputIds, scope.anchorIds);
}
