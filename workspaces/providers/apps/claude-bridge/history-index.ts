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
import type { HistoryRecordKind } from "./history-records.ts";
import { historyRecordKind } from "./history-records.ts";
import { isDeepStrictEqual } from "node:util";
import path from "node:path";
import {
  readNativeHistory,
  nativeHistoryRows,
  nativeUuid,
} from "./native-history.ts";

export interface HistoryIndex {
  readonly messages: readonly sdk.SessionMessage[];
  readonly positions: (ids: readonly string[]) => number[] | null;
  readonly outcome: (
    ids: readonly string[] | null,
    anchors: readonly string[] | null,
  ) => "unknown" | "delivered" | "not_applied";
}
export interface VerifiedHistoryRecord {
  readonly row: sdk.SessionStoreEntry;
  readonly kind: HistoryRecordKind;
  // Nearest user/assistant ancestor, through validated non-conversation rows.
  readonly parentMessageId: string | null;
}

/** One UUID lookup for a complete history snapshot, shared by controls and recovery.
 */
export function messageIndex(
  messages: readonly sdk.SessionMessage[],
  inputIds: ReadonlySet<string> = new Set(
    messages
      .filter((message) => message.type === "user")
      .map((message) => message.uuid),
  ),
  uncertainIds: ReadonlySet<string> = new Set(),
  // False when an opaque record could be an input this index cannot see.
  absenceProvable = true,
): HistoryIndex {
  const positions: Map<string, number> = new Map();
  for (const [position, message] of messages.entries())
    positions.set(message.uuid, positions.has(message.uuid) ? -1 : position);
  return {
    messages,
    positions(ids: readonly string[]) {
      const found = ids.map((id) => positions.get(id) ?? -1);
      return found.every(
        (position, i) =>
          position >= 0 && (i === 0 || position > (found[i - 1] ?? -1)),
      )
        ? found
        : null;
    },
    outcome(ids: readonly string[] | null, anchors: readonly string[] | null) {
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
      return absenceProvable &&
        ids.every((id) => !inputIds.has(id) && !uncertainIds.has(id))
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
function verifiedChain(rows: sdk.SessionStoreEntry[], sessionId: string) {
  const chain: ChainEntry[] = [];
  const seen = new Set();
  const ancestry = new Map<
    string,
    { inputId: string | null; messageId: string | null }
  >();
  const records: VerifiedHistoryRecord[] = [];
  let previousInput: string | null = null;
  const inputIds = new Set<string>();
  const uncertainIds = new Set<string>();
  let absenceProvable = true;
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
    if (kind === "opaque") absenceProvable = false;
    if (kind === "queue" || kind === "metadata")
      records.push({ row, kind, parentMessageId: null });
    if (kind === "queue") {
      if (row.sessionId !== sessionId) return null;
      if (nativeUuid(row.commandUuid)) uncertainIds.add(row.commandUuid);
      continue;
    }
    if (row.isSidechain === true || kind === "metadata") continue;
    // Claude records the shell's current directory per row, which may change.
    if (
      !nativeUuid(row.uuid) ||
      row.sessionId !== sessionId ||
      typeof row.cwd !== "string" ||
      !path.isAbsolute(row.cwd) ||
      row.isSidechain !== false
    )
      return null;
    if (
      row.parentUuid !== null &&
      (typeof row.parentUuid !== "string" || !ancestry.has(row.parentUuid))
    )
      return null;
    const ancestor =
      row.parentUuid === null ? null : (ancestry.get(row.parentUuid) ?? null);
    // Inputs must belong to one proven conversation; output/progress may be
    // siblings and carry evolving payloads without changing input coverage.
    if (
      kind === "input" &&
      previousInput !== null &&
      ancestor?.inputId !== previousInput
    )
      return null;
    const parentMessageId = ancestor?.messageId ?? null;
    records.push({ row, kind, parentMessageId });
    ancestry.set(row.uuid, {
      inputId: kind === "input" ? row.uuid : (ancestor?.inputId ?? null),
      messageId:
        row.type === "user" || row.type === "assistant"
          ? row.uuid
          : parentMessageId,
    });
    if (kind === "input") previousInput = row.uuid;
    if (row.type === "user" || row.type === "assistant") {
      chain.push(row as ChainEntry);
      if (kind === "input") inputIds.add(row.uuid);
    }
    if (row.type === "attachment" && kind === "input") {
      const attachment = row.attachment as { source_uuid: string };
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
  return chain.length
    ? Object.assign(chain, {
        inputIds,
        uncertainIds,
        absenceProvable,
        records,
      })
    : null;
}

export async function readHistoryIndex(
  session: HistorySession,
  options: HistoryReadOptions = {},
): Promise<
  (HistoryIndex & { readonly records: readonly VerifiedHistoryRecord[] }) | null
> {
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
      const entries: sdk.SessionStoreEntry[] = [];
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
      const chain = verifiedChain(entries, sessionId);
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
            chain.absenceProvable,
          ),
          { records: chain.records },
        ),
      ];
    });
    return result?.[0] ?? null;
  } catch {
    return null;
  }
}

/** Controls cannot perform a native effect from uncertain coverage.
 */
export async function verifiedHistoryIndex(session: HistorySession) {
  const index = await readHistoryIndex(session);
  if (!index)
    throw new Error(
      "The Claude history is incomplete or uncertain; no native history operation was submitted",
    );
  return index;
}

/** Pure integrity witness for SDK store entries. Production readers also require
 * pinned bytes, SDK metadata, and the typed SDK conversation chain.
 */
export function inputHistoryOutcome(
  rows: unknown,
  scope: {
    sessionId: string;
    cwd: string;
    anchorIds: string[] | null;
    inputIds: string[] | null;
  },
) {
  if (!Array.isArray(rows)) return "unknown";
  const chain = verifiedChain(rows as sdk.SessionStoreEntry[], scope.sessionId);
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
    chain.absenceProvable,
  ).outcome(scope.inputIds, scope.anchorIds);
}
