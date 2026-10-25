// @ts-check
import * as sdk from "@anthropic-ai/claude-agent-sdk";
import { isDeepStrictEqual } from "node:util";
import {
  readNativeHistory,
  nativeHistoryRows,
  nativeUuid,
} from "./native-history.mjs";

/** One UUID lookup for a complete history snapshot, shared by controls and recovery.
 * @param {sdk.SessionMessage[]} messages
 */
export function messageIndex(messages) {
  /** @type {Map<string, number>} */
  const positions = new Map();
  for (const [position, message] of messages.entries())
    positions.set(message.uuid, positions.has(message.uuid) ? -1 : position);
  return {
    messages,
    /** @param {string[]} ids */
    positions(ids) {
      const found = ids.map((id) => positions.get(id) ?? -1);
      return found.every(
        (position, i) =>
          position >= 0 && (i === 0 || position > (found[i - 1] ?? -1)),
      )
        ? found
        : null;
    },
    /** @param {string[] | null} ids @param {string[] | null} anchors */
    outcome(ids, anchors) {
      if (
        !ids?.length ||
        !anchors?.length ||
        !ids.every(nativeUuid) ||
        !anchors.every(nativeUuid) ||
        new Set(ids).size !== ids.length ||
        new Set(anchors).size !== anchors.length ||
        !this.positions(anchors) ||
        !this.positions(anchors)?.every(
          (position) => messages[position]?.type === "user",
        )
      )
        return "unknown";
      const found = this.positions(ids);
      if (
        found &&
        found.every((position) => messages[position]?.type === "user")
      )
        return "delivered";
      return ids.every((id) => !positions.has(id)) ? "not_applied" : "unknown";
    },
  };
}

// The SDK deliberately omits hidden meta inputs from getSessionMessages. Its
// typed SessionStore import retains those inputs without changing their prompt.
// The raw reader is only an integrity guard: SDK parsers silently discard bad
// records. A stable, complete, strictly recognized file is required before any
// absence or branch can be trusted. Unknown CLI formats fail closed.
const CHAIN_TYPES = new Set(["user", "assistant", "attachment", "system"]);
const ATTACHMENT_TYPES = new Set([
  "queued_command",
  "environment",
  "model",
  "total_tokens_reminder",
  "session_context",
  "date",
  "prompt_snapshot",
]);
const AUXILIARY_TYPES = new Set([
  "progress",
  "file-history-snapshot",
  "queue-operation",
  "last-prompt",
  "custom-title",
  "agent-name",
  "agent-color",
  "tag",
  "mode",
  "pr-link",
  "atis-latch",
  "api-request-shape",
  "api-request-blob",
  "api-request",
  "cost-state",
]);
/** @param {sdk.SessionStoreEntry[]} rows @param {string} sessionId @param {string} cwd */
function verifiedChain(rows, sessionId, cwd) {
  /** @type {sdk.SessionStoreEntry[]} */
  const chain = [];
  const seen = new Set();
  let previous = null;
  for (const row of rows) {
    if (
      !row ||
      typeof row !== "object" ||
      Array.isArray(row) ||
      typeof row.type !== "string"
    )
      return null;
    if (!CHAIN_TYPES.has(row.type) && !AUXILIARY_TYPES.has(row.type))
      return null;
    if (row.uuid !== undefined) {
      if (!nativeUuid(row.uuid) || seen.has(row.uuid)) return null;
      seen.add(row.uuid);
    }
    if (
      row.isCompactSummary ||
      row.type === "summary" ||
      row.subtype === "compact_boundary"
    )
      return null;
    if (row.isSidechain === true) continue;
    if (AUXILIARY_TYPES.has(row.type)) continue;
    if (
      !nativeUuid(row.uuid) ||
      row.sessionId !== sessionId ||
      row.cwd !== cwd ||
      row.isSidechain !== false ||
      row.parentUuid !== previous
    )
      return null;
    if (
      row.type === "attachment" &&
      (!row.attachment ||
        typeof row.attachment !== "object" ||
        !("type" in row.attachment) ||
        typeof row.attachment.type !== "string" ||
        !ATTACHMENT_TYPES.has(row.attachment.type))
    )
      return null;
    previous = row.uuid;
    if (row.type === "user" || row.type === "assistant") {
      const body = row.message;
      if (
        !body ||
        typeof body !== "object" ||
        Array.isArray(body) ||
        !("role" in body) ||
        body.role !== row.type ||
        !("content" in body) ||
        !(typeof body.content === "string" || Array.isArray(body.content))
      )
        return null;
      chain.push(row);
    }
    if (
      row.type === "attachment" &&
      row.attachment &&
      typeof row.attachment === "object" &&
      "type" in row.attachment &&
      row.attachment.type === "queued_command"
    ) {
      if (
        !("source_uuid" in row.attachment) ||
        !nativeUuid(row.attachment.source_uuid)
      )
        return null;
      // A queued attachment is positive evidence, even when the CLI absorbed its
      // text. It is never guessed to be absent from the filtered SDK view.
      chain.push({
        ...row,
        queuedProvenance: true,
        type: "user",
        uuid: /** @type {string} */ (row.attachment.source_uuid),
        message: { role: "user", content: [] },
      });
    }
  }
  return chain.length ? chain : null;
}

/** @typedef {{id: string, nativeId?: string, cwd: string, nativeHistoryCwd?: string}} HistorySession */
/** @typedef {{configDir?: string, filesystem?: object, sessionApi?: Pick<typeof sdk, "getSessionInfo" | "getSessionMessages" | "importSessionToStore">}} HistoryReadOptions */
/** @param {HistorySession} session @param {HistoryReadOptions} [options] */
export async function readHistoryIndex(session, options = {}) {
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
      const entries = [];
      /** @type {sdk.SessionStore} */
      const store = {
        append: async (key, batch) => {
          if (key.sessionId !== sessionId || key.subpath)
            throw new Error("Unexpected Claude history scope");
          entries.push(...batch);
        },
        load: async () => null,
      };
      await api.importSessionToStore(sessionId, store, {
        dir,
        includeSubagents: false,
      });
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
        .filter((row) => visibleIds.has(/** @type {string} */ (row.uuid)))
        .map((row) => row.uuid);
      if (!isDeepStrictEqual(visibleOrder, expectedOrder)) return null;
      if (
        chain.some(
          (row) =>
            !row.isMeta &&
            !row.queuedProvenance &&
            row.type !== "system" &&
            !visibleIds.has(/** @type {string} */ (row.uuid)),
        )
      )
        return null;
      const after = await api.getSessionInfo(sessionId, { dir });
      if (!isDeepStrictEqual(before, after)) return null;
      return [
        Object.assign(
          messageIndex(
            chain.map((row) => ({
              type: /** @type {"user" | "assistant"} */ (row.type),
              uuid: /** @type {string} */ (row.uuid),
              session_id: sessionId,
              message: row.message,
              parent_tool_use_id: null,
              parent_agent_id: null,
            })),
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
export async function verifiedHistoryMessages(session) {
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
export function inputHistoryOutcome(rows, scope) {
  if (!Array.isArray(rows)) return "unknown";
  const chain = verifiedChain(rows, scope.sessionId, scope.cwd);
  if (!chain) return "unknown";
  return messageIndex(
    chain.map((row) => ({
      type: /** @type {"user" | "assistant"} */ (row.type),
      uuid: /** @type {string} */ (row.uuid),
      session_id: scope.sessionId,
      message: row.message,
      parent_tool_use_id: null,
      parent_agent_id: null,
    })),
  ).outcome(scope.inputIds, scope.anchorIds);
}
