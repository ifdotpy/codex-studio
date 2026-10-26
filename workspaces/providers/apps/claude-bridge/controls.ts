import assert from "node:assert/strict";
import type {
  SessionMessage,
  SlashCommand,
  ForkSessionOptions,
  ForkSessionResult,
  GetSessionMessagesOptions,
  RewindFilesResult,
} from "@anthropic-ai/claude-agent-sdk";
import type { Session, Turn, FileRestorePlan } from "./session-state.ts";
import type {
  BridgeCommand,
  BridgeParams,
  BridgeSessionParams,
} from "./bridge-protocol.ts";
export interface ControlDependencies {
  persist(session: Session): Promise<unknown>;
  getHistoryIndex(
    id: string,
    options?: GetSessionMessagesOptions,
  ): Promise<HistoryIndex>;
  forkSession(
    id: string,
    options: ForkSessionOptions,
  ): Promise<ForkSessionResult>;
  rewindFiles(
    id: string,
    messageId: string,
    options: { dryRun: boolean },
  ): Promise<RewindFilesResult>;
  prepareFileRestore(
    session: Session,
    messages: readonly SessionMessage[],
    messageId: string,
  ): Promise<FileRestorePlan>;
  verifyFileRestore(
    plan: FileRestorePlan,
    stage: "expected" | "before",
  ): Promise<void>;
  randomUUID?: () => string;
}
type Command = Pick<SlashCommand, "name"> & Partial<SlashCommand>;
import { checkpointSourceMatches } from "./file-checkpoints.ts";
import { messageIndex } from "./history-index.ts";
import type { HistoryIndex } from "./history-index.ts";
import { nativeInputIds } from "./input-messages.ts";
import { randomUUID } from "node:crypto";

// Initialization is the CLI's authoritative list after native skill policy.
export function commandCatalog(commands: readonly Command[] = []) {
  const named = new Map<string, BridgeCommand>();
  for (const row of commands) {
    if (!row || !/^[\w:.-]+$/.test(row.name || "")) continue;
    if (!named.has(row.name) || row.builtin)
      named.set(row.name, {
        name: row.name,
        description: String(row.description || ""),
        argumentHint: String(row.argumentHint || ""),
        builtin: !!row.builtin,
        aliases: (row.aliases || []).filter((alias) =>
          /^[\w:.-]+$/.test(alias),
        ),
      });
  }
  return [...named.values()];
}

// Only an explicit command is transformed. Never extract commands from prose.
export function explicitNativeCommand(
  text: unknown,
  commands: readonly Command[],
) {
  if (typeof text !== "string" || /[\r\n\0]/.test(text)) return null;
  const match = /^([/$])([\w:.-]+)(?:[ \t]+(.*))?$/.exec(text.trim());
  if (!match) return null;
  const catalog = commandCatalog(commands);
  const row =
    catalog.find((row) => row.name === match[2]) ||
    catalog.find((row) => row.aliases.includes(match[2] ?? ""));
  if (!row || (match[1] === "$" && row.builtin)) return null;
  return `/${row.name}${match[3] ? ` ${match[3]}` : ""}`;
}

// /clear keeps the Studio mirror, but earlier conversations and the local
// clear command have no boundary in the new native session.
function currentNativeTurns(turns: Turn[], nativeId: string) {
  return turns.filter(
    (turn) =>
      !turn.nativeHistoryDetached &&
      (!turn.nativeSessionId || turn.nativeSessionId === nativeId),
  );
}
// SDK SessionMessage UUIDs identify every part of one logical input. The
// first part is the history boundary; the last can own a file checkpoint.
function nativeParts(turn: Turn) {
  const input = turn.items?.find((item) => item.type === "userMessage");
  // Older forks remapped only nativeMessageId. Without a composite receipt,
  // that SDK-verified boundary remains authoritative over the original item.
  const ids = input?.nativeIds
    ? nativeInputIds(input)
    : [turn.nativeMessageId || input?.nativeId || turn.id];
  if (!ids || (turn.nativeMessageId && !ids.includes(turn.nativeMessageId)))
    throw new Error("The exact Claude turn boundary is unavailable");
  return ids;
}
function partPositions(turn: Turn, index: HistoryIndex) {
  const { messages } = index;
  const positions: number[] | null = index.positions(nativeParts(turn));
  if (
    !positions ||
    positions.some((position) => {
      const message = messages[position];
      return (
        message?.type !== "user" ||
        message.parent_tool_use_id ||
        message.parent_agent_id
      );
    })
  )
    throw new Error(
      "The exact Claude turn boundary is unavailable after compaction or history recovery",
    );
  return positions;
}
function turnPositions(turns: Turn[], index: HistoryIndex) {
  const positions = turns.map((turn) => partPositions(turn, index));
  if (
    positions.some(
      (parts, i) =>
        i > 0 && (parts[0] ?? -1) <= (positions[i - 1]?.at(-1) ?? -1),
    )
  )
    throw new Error(
      "The exact Claude turn boundary is unavailable after compaction or history recovery",
    );
  return positions;
}
function remapTurn(turn: Turn, source: HistoryIndex, fork: HistoryIndex) {
  const forkMessages = fork.messages;
  const initialIds = nativeParts(turn);
  const first = partPositions(turn, source)[0];
  const forkFirst = first === undefined ? undefined : forkMessages[first];
  assert.ok(forkFirst, "The exact Claude turn boundary is unavailable");
  turn.nativeMessageId = forkFirst.uuid;
  const initial = turn.items?.find((item) => item.type === "userMessage");
  for (const item of turn.items || []) {
    if (item.type !== "userMessage") continue;
    // A withdrawn input has a logical receipt but no native history to remap.
    if (
      item.delivery === "steer" &&
      ["not_applied", "cancelled"].includes(item.deliveryStatus ?? "")
    )
      continue;
    // Every retained non-initial user input needs a native identity, even
    // legacy items without a delivery marker. Withdrawals were exempted above.
    if (!item.nativeId && item !== initial)
      throw new Error("Claude retained input mapping is unavailable");
    const ids = item === initial ? initialIds : nativeInputIds(item);
    if (!ids) throw new Error("Claude fork lost a retained input boundary");
    const remapped = ids.map((id) => {
      const index = source.positions([id])?.[0] ?? -1;
      if (index < 0 || !forkMessages[index]?.uuid)
        throw new Error("Claude fork lost a retained input boundary");
      const message = forkMessages[index];
      assert.ok(message);
      return message.uuid;
    });
    const last = remapped.at(-1);
    assert.ok(last);
    item.nativeId = last;
    if (item.nativeIds) item.nativeIds = remapped;
  }
  // A fork remaps message IDs. Its SDK backups have not been verified.
  delete turn.fileCheckpointMessageId;
  delete turn.fileCheckpointSource;
}
// Validate every retained input against the authoritative SDK rows before
// creating a native branch. Legacy steers without supported provenance cannot
// be guessed from text or position; refuse the operation before any effect.
function preflightRetained(turns: Turn[], index: HistoryIndex) {
  try {
    for (const turn of turns) remapTurn(structuredClone(turn), index, index);
  } catch (cause) {
    throw new Error(
      "The Claude input mapping is unavailable; no native history operation was submitted",
      { cause },
    );
  }
}
function comparable(message: SessionMessage | undefined) {
  return JSON.stringify({ type: message?.type, message: message?.message });
}

export function rollbackBoundary(
  turns: Turn[],
  history: HistoryIndex,
  turnId: string,
) {
  const index = turns.findIndex((turn) => turn.id === turnId);
  if (index < 0) throw new Error("The selected Claude turn does not exist");
  const positions = turnPositions(turns.slice(0, index + 1), history);
  const end = positions[index]?.[0];
  assert.ok(end !== undefined);
  return {
    index,
    end,
    upToMessageId: index ? (history.messages[end - 1]?.uuid ?? null) : null,
  };
}

// Caller must serialize session mutations and close its idle SDK query first.
// persist must atomically save the whole descriptor. Never retry an unknown fork.
export async function rollbackSession(
  session: Session,
  request: {
    requestId: string;
    turnId: string;
    restoreFiles?: boolean | undefined;
  },
  deps: ControlDependencies,
) {
  const { requestId, turnId } = request;
  const restoreFiles = request.restoreFiles === true;
  if (typeof requestId !== "string" || !requestId.trim())
    throw new Error("A rollback request identity is required");
  if (typeof turnId !== "string" || !turnId)
    throw new Error("A Claude turn identity is required");
  session.controlRequests ||= {};
  let receipt = Object.hasOwn(session.controlRequests, requestId)
    ? session.controlRequests[requestId]
    : undefined;
  if (
    receipt &&
    (receipt.kind !== "rollback" ||
      receipt.turnId !== turnId ||
      !!receipt.restoreFiles !== restoreFiles)
  )
    throw new Error("This request identity has different content");
  if (receipt?.status === "completed") {
    assert.ok(receipt.result);
    await deps.persist(session);
    return receipt.result;
  }
  if (["submitted", "rewind_submitted"].includes(receipt?.status ?? ""))
    throw new Error(
      "The Claude history operation outcome is unknown; this request cannot repeat it",
    );
  if (session.turns.some((turn) => turn.status === "inProgress"))
    throw new Error("Pause Claude before rollback");
  if (
    Object.entries(session.controlRequests).some(
      ([id, value]) => id !== requestId && value.status !== "completed",
    )
  )
    throw new Error("Another Claude history operation needs recovery");
  let sourceHistory: HistoryIndex | undefined;
  if (!receipt) {
    const sourceNativeId = session.nativeId || session.id;
    const history = await deps.getHistoryIndex(sourceNativeId, {
      dir: session.nativeHistoryCwd || session.cwd,
    });
    sourceHistory = history;
    const { messages } = history;
    const nativeTurns = currentNativeTurns(session.turns, sourceNativeId);
    const boundary = rollbackBoundary(nativeTurns, history, turnId);
    const mirrorIndex = session.turns.findIndex((turn) => turn.id === turnId);
    const previousTurns = structuredClone(session.turns);
    preflightRetained(nativeTurns.slice(0, boundary.index), history);
    const selectedTurn = session.turns[mirrorIndex];
    assert.ok(selectedTurn);
    const fileMessageId = selectedTurn.fileCheckpointMessageId;
    let filePlan;
    if (restoreFiles) {
      if (
        !fileMessageId ||
        fileMessageId !== nativeParts(selectedTurn).at(-1) ||
        !checkpointSourceMatches(session, selectedTurn)
      )
        throw new Error("This message has no SDK file checkpoint");
      const preview = await deps.rewindFiles(sourceNativeId, fileMessageId, {
        dryRun: true,
      });
      if (!preview?.canRewind)
        throw new Error(
          preview?.error || "The SDK file checkpoint is unavailable",
        );
      try {
        filePlan = await deps.prepareFileRestore(
          session,
          messages,
          fileMessageId,
        );
      } catch (error) {
        // This phase has no native fork or file effects and no durable receipt.
        throw Object.assign(
          new Error(error instanceof Error ? error.message : String(error), {
            cause: error,
          }),
          {
            data: { fileRestoreOutcome: "not_applied", requestId, turnId },
          },
        );
      }
    }
    receipt = {
      ...(filePlan === undefined ? {} : { filePlan }),
      kind: "rollback",
      turnId,
      restoreFiles,
      ...(fileMessageId === undefined ? {} : { fileMessageId }),
      status: "submitted",
      sourceNativeId,
      createdAt: new Date().toISOString(),
      retainedCount: mirrorIndex,
      sourceMessages: messages.slice(0, boundary.end),
      previousTurns,
      upToMessageId: boundary.upToMessageId,
    };
    Object.defineProperty(session.controlRequests, requestId, {
      value: receipt,
      enumerable: true,
      writable: true,
      configurable: true,
    });
    await deps.persist(session);
    const result = boundary.index
      ? await deps.forkSession(sourceNativeId, {
          dir: session.nativeHistoryCwd || session.cwd,
          ...(boundary.upToMessageId === null
            ? {}
            : { upToMessageId: boundary.upToMessageId }),
        })
      : { sessionId: (deps.randomUUID || randomUUID)() };
    if (!result?.sessionId || result.sessionId === sourceNativeId)
      throw new Error("Claude returned an invalid fork receipt");
    receipt = session.controlRequests[requestId] = {
      ...receipt,
      targetNativeId: result.sessionId,
      status: "forked",
    };
    await deps.persist(session);
  }
  if (receipt.status !== "forked" && receipt.status !== "rewound")
    throw new Error("Unsupported Claude rollback receipt");
  assert.ok(Array.isArray(receipt.previousTurns));
  assert.ok(receipt.targetNativeId);
  const retained = structuredClone(
    receipt.previousTurns.slice(0, receipt.retainedCount),
  );
  const nativeRetained = currentNativeTurns(retained, receipt.sourceNativeId);
  if (nativeRetained.length) {
    const fork = await deps.getHistoryIndex(receipt.targetNativeId, {
      dir: session.nativeHistoryCwd || session.cwd,
    });
    const forkMessages = fork.messages;
    const source = receipt.sourceMessages;
    assert.ok(Array.isArray(source));
    if (
      source.length !== forkMessages.length ||
      source.some(
        (message, i) => comparable(message) !== comparable(forkMessages[i]),
      )
    )
      throw new Error(
        "Claude fork history did not preserve the retained messages",
      );
    const sourceIndex = sourceHistory ?? messageIndex(source);
    for (const turn of nativeRetained) {
      remapTurn(turn, sourceIndex, fork);
      turn.nativeSessionId = receipt.targetNativeId;
    }
  }
  if (restoreFiles && receipt.status === "rewound") {
    assert.ok(receipt.filePlan);
    await deps.verifyFileRestore(receipt.filePlan, "expected");
  }
  if (restoreFiles && receipt.status !== "rewound") {
    assert.ok(receipt.filePlan);
    assert.ok(receipt.fileMessageId);
    await deps.verifyFileRestore(receipt.filePlan, "before");
    receipt = session.controlRequests[requestId] = {
      ...receipt,
      restoreFiles: true,
      filePlan: receipt.filePlan,
      fileMessageId: receipt.fileMessageId,
      status: "rewind_submitted",
    };
    await deps.persist(session);
    const result = await deps.rewindFiles(
      receipt.sourceNativeId,
      receipt.fileMessageId,
      { dryRun: false },
    );
    if (!result?.canRewind || (result.skippedLinks ?? 0) > 0)
      throw new Error(
        result?.error ||
          "Claude could not restore every tracked file; inspect workspace recovery",
      );
    await deps.verifyFileRestore(receipt.filePlan, "expected");
    receipt = session.controlRequests[requestId] = {
      ...receipt,
      fileResult: result,
      status: "rewound",
    };
    await deps.persist(session);
  }
  session.historyBranches ||= [];
  session.historyBranches.push({
    requestId,
    nativeId: receipt.sourceNativeId,
    turns: receipt.previousTurns,
  });
  session.nativeId = receipt.targetNativeId;
  session.started = nativeRetained.length > 0;
  if (!nativeRetained.length) session.nativeHistoryCwd = session.cwd;
  session.turns = retained;
  const result = {
    threadId: session.id,
    nativeId: session.nativeId,
    removedTurns: receipt.previousTurns.length - retained.length,
    ...(restoreFiles
      ? {
          filesRestored: true,
          ...(receipt.fileResult === undefined
            ? {}
            : { fileResult: receipt.fileResult }),
        }
      : {}),
  };
  const {
    sourceMessages: _sourceMessages,
    previousTurns: _previousTurns,
    ...completed
  } = receipt;
  receipt = session.controlRequests[requestId] = {
    ...completed,
    status: "completed",
    result,
  };
  await deps.persist(session);
  return result;
}

// The Runtime workspace operation owns submission identity and unknown outcomes.
export async function forkAtTurn(
  source: Session,
  params: BridgeSessionParams &
    Partial<
      Pick<
        BridgeParams<"thread/fork">,
        "threadId" | "lastTurnId" | "excludeTurns"
      >
    >,
  deps: Pick<
    ControlDependencies,
    "persist" | "getHistoryIndex" | "forkSession" | "randomUUID"
  >,
) {
  // Native reads can wait while the source receives more output. Keep the
  // branch descriptor and its boundary independent of those later changes.
  source = structuredClone(source);
  const selected =
    params.lastTurnId == null
      ? source.turns.length - 1
      : source.turns.findIndex((turn) => turn.id === params.lastTurnId);
  if (params.lastTurnId != null && selected < 0)
    throw new Error("The selected Claude turn does not exist");
  const retained = source.turns.slice(0, selected + 1);
  if (
    retained.some((turn) =>
      ["inProgress", "uncertain"].includes(turn.status ?? ""),
    ) ||
    (selected >= 0 && source.turns[selected]?.status !== "completed")
  )
    throw new Error("Select a completed Claude turn before branching");
  let targetNativeId;
  if (!source.started && !source.turns.length) {
    targetNativeId = (deps.randomUUID || randomUUID)();
  } else {
    const nativeId = source.nativeId || source.id;
    const dir = source.nativeHistoryCwd || source.cwd;
    const history = await deps.getHistoryIndex(nativeId, { dir });
    const { messages } = history;
    const nativeTurns = currentNativeTurns(source.turns, nativeId);
    const nativeSelected = nativeTurns.findIndex(
      (turn) => turn.id === source.turns[selected]?.id,
    );
    if (nativeSelected < 0)
      throw new Error(
        "The selected Claude turn belongs to an earlier native session or a local session transition",
      );
    const nativeRetained = currentNativeTurns(retained, nativeId);
    const positions = turnPositions(
      nativeTurns.slice(0, nativeSelected + 2),
      history,
    );
    const end =
      nativeSelected < nativeTurns.length - 1
        ? (positions[nativeSelected + 1]?.[0] ?? -1)
        : messages.length;
    if (
      end < 1 ||
      positions
        .slice(0, nativeRetained.length)
        .some((parts) => (parts.at(-1) ?? -1) >= end)
    )
      throw new Error(
        "The exact Claude branch boundary is unavailable after compaction or history recovery",
      );
    preflightRetained(nativeRetained, history);
    const fork = await deps.forkSession(nativeId, {
      dir,
      ...(params.lastTurnId != null
        ? { upToMessageId: messages[end - 1]?.uuid ?? "" }
        : {}),
    });
    targetNativeId = fork?.sessionId;
    if (!targetNativeId || targetNativeId === nativeId)
      throw new Error("Claude returned an invalid fork receipt");
    const forkHistory = await deps.getHistoryIndex(targetNativeId, { dir });
    const forkMessages = forkHistory.messages;
    if (
      forkMessages.length !== end ||
      messages
        .slice(0, end)
        .some(
          (message, i) => comparable(message) !== comparable(forkMessages[i]),
        )
    )
      throw new Error(
        "Claude fork history did not preserve the selected messages",
      );
    for (const turn of nativeRetained) {
      remapTurn(turn, history, forkHistory);
      turn.nativeSessionId = targetNativeId;
    }
  }
  const {
    controlRequests: _controlRequests,
    historyBranches: _historyBranches,
    ...base
  } = source;
  const { threadId: _threadId, lastTurnId: _lastTurnId, ...settings } = params;
  const target = {
    ...base,
    ...settings,
    cwd: settings.cwd ?? source.cwd,
    id: targetNativeId,
    nativeId: targetNativeId,
    nativeHistoryCwd:
      !source.started && !source.turns.length
        ? settings.cwd || source.cwd
        : source.nativeHistoryCwd || source.cwd,
    turns: retained,
  };
  await deps.persist(target);
  return target;
}
