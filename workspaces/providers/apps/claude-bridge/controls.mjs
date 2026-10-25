import { checkpointSourceMatches } from "./file-checkpoints.mjs";
import { nativeInputIds } from "./input-messages.mjs";
import { randomUUID } from "node:crypto";

// Initialization is the CLI's authoritative list after native skill policy.
export function commandCatalog(commands = []) {
  const named = new Map();
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
export function explicitNativeCommand(text, commands) {
  if (typeof text !== "string" || /[\r\n\0]/.test(text)) return null;
  const match = /^([/$])([\w:.-]+)(?:[ \t]+(.*))?$/.exec(text.trim());
  if (!match) return null;
  const catalog = commandCatalog(commands);
  const row =
    catalog.find((row) => row.name === match[2]) ||
    catalog.find((row) => row.aliases.includes(match[2]));
  if (!row || (match[1] === "$" && row.builtin)) return null;
  return `/${row.name}${match[3] ? ` ${match[3]}` : ""}`;
}

// SDK SessionMessage UUIDs identify every part of one logical input. The
// first part is the history boundary; the last can own a file checkpoint.
function nativeParts(turn) {
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
/** @param {import("@anthropic-ai/claude-agent-sdk").SessionMessage[]} messages */
function partPositions(turn, messages) {
  const positions = nativeParts(turn).map((id) => {
    const matches = messages.flatMap((message, index) =>
      message.uuid === id ? [index] : [],
    );
    if (matches.length !== 1) return -1;
    const message = messages[matches[0]];
    return message.type === "user" &&
      !message.parent_tool_use_id &&
      !message.parent_agent_id
      ? matches[0]
      : -1;
  });
  if (
    positions.some(
      (position, i) => position < 0 || (i > 0 && position <= positions[i - 1]),
    )
  )
    throw new Error(
      "The exact Claude turn boundary is unavailable after compaction or history recovery",
    );
  return positions;
}
function turnPositions(turns, messages) {
  const positions = turns.map((turn) => partPositions(turn, messages));
  if (
    positions.some((parts, i) => i > 0 && parts[0] <= positions[i - 1].at(-1))
  )
    throw new Error(
      "The exact Claude turn boundary is unavailable after compaction or history recovery",
    );
  return positions;
}
function remapTurn(turn, source, forkMessages) {
  const initialIds = nativeParts(turn);
  const first = partPositions(turn, source)[0];
  turn.nativeMessageId = forkMessages[first].uuid;
  const initial = turn.items?.find((item) => item.type === "userMessage");
  for (const item of turn.items || []) {
    if (item.type !== "userMessage") continue;
    // A withdrawn input has a logical receipt but no native history to remap.
    if (
      item.delivery === "steer" &&
      ["not_applied", "cancelled"].includes(item.deliveryStatus)
    )
      continue;
    // Every retained non-initial user input needs a native identity, even
    // legacy items without a delivery marker. Withdrawals were exempted above.
    if (!item.nativeId && item !== initial)
      throw new Error("Claude retained input mapping is unavailable");
    const ids = item === initial ? initialIds : nativeInputIds(item);
    if (!ids) throw new Error("Claude fork lost a retained input boundary");
    const remapped = ids.map((id) => {
      const index = source.findIndex((message) => message.uuid === id);
      if (index < 0 || !forkMessages[index]?.uuid)
        throw new Error("Claude fork lost a retained input boundary");
      return forkMessages[index].uuid;
    });
    item.nativeId = remapped.at(-1);
    if (item.nativeIds) item.nativeIds = remapped;
  }
  // A fork remaps message IDs. Its SDK backups have not been verified.
  delete turn.fileCheckpointMessageId;
  delete turn.fileCheckpointSource;
}
// Validate every retained input against the authoritative SDK rows before
// creating a native branch. Legacy steers without supported provenance cannot
// be guessed from text or position; refuse the operation before any effect.
function preflightRetained(turns, messages) {
  try {
    for (const turn of turns)
      remapTurn(structuredClone(turn), messages, messages);
  } catch (cause) {
    throw new Error(
      "The Claude input mapping is unavailable; no native history operation was submitted",
      { cause },
    );
  }
}
function comparable(message) {
  return JSON.stringify({ type: message.type, message: message.message });
}

export function rollbackBoundary(turns, messages, turnId) {
  const index = turns.findIndex((turn) => turn.id === turnId);
  if (index < 0) throw new Error("The selected Claude turn does not exist");
  const positions = turnPositions(turns.slice(0, index + 1), messages);
  const end = positions[index][0];
  return { index, end, upToMessageId: index ? messages[end - 1]?.uuid : null };
}

// Caller must serialize session mutations and close its idle SDK query first.
// persist must atomically save the whole descriptor. Never retry an unknown fork.
export async function rollbackSession(session, request, deps) {
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
    await deps.persist(session);
    return receipt.result;
  }
  if (["submitted", "rewind_submitted"].includes(receipt?.status))
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
  if (!receipt) {
    const sourceNativeId = session.nativeId || session.id;
    const messages = await deps.getSessionMessages(sourceNativeId, {
      dir: session.nativeHistoryCwd || session.cwd,
    });
    const boundary = rollbackBoundary(session.turns, messages, turnId);
    const previousTurns = structuredClone(session.turns);
    preflightRetained(previousTurns.slice(0, boundary.index), messages);
    const fileMessageId = session.turns[boundary.index].fileCheckpointMessageId;
    let filePlan;
    if (restoreFiles) {
      if (
        !fileMessageId ||
        fileMessageId !== nativeParts(session.turns[boundary.index]).at(-1) ||
        !checkpointSourceMatches(session, session.turns[boundary.index])
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
        throw Object.assign(new Error(error.message, { cause: error }), {
          data: { fileRestoreOutcome: "not_applied", requestId, turnId },
        });
      }
    }
    receipt = {
      ...(restoreFiles ? { filePlan } : {}),
      kind: "rollback",
      turnId,
      restoreFiles,
      fileMessageId,
      status: "submitted",
      sourceNativeId,
      createdAt: new Date().toISOString(),
      retainedCount: boundary.index,
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
          upToMessageId: boundary.upToMessageId,
        })
      : { sessionId: (deps.randomUUID || randomUUID)() };
    if (!result?.sessionId || result.sessionId === sourceNativeId)
      throw new Error("Claude returned an invalid fork receipt");
    receipt.targetNativeId = result.sessionId;
    receipt.status = "forked";
    await deps.persist(session);
  }
  if (!["forked", "rewound"].includes(receipt.status))
    throw new Error("Unsupported Claude rollback receipt");
  const retained = structuredClone(
    receipt.previousTurns.slice(0, receipt.retainedCount),
  );
  if (retained.length) {
    const forkMessages = await deps.getSessionMessages(receipt.targetNativeId, {
      dir: session.nativeHistoryCwd || session.cwd,
    });
    const source = receipt.sourceMessages;
    if (
      source.length !== forkMessages.length ||
      source.some(
        (message, i) => comparable(message) !== comparable(forkMessages[i]),
      )
    )
      throw new Error(
        "Claude fork history did not preserve the retained messages",
      );
    for (const turn of retained) remapTurn(turn, source, forkMessages);
  }
  if (restoreFiles && receipt.status === "rewound")
    await deps.verifyFileRestore(receipt.filePlan, "expected");
  if (restoreFiles && receipt.status !== "rewound") {
    await deps.verifyFileRestore(receipt.filePlan, "before");
    receipt.status = "rewind_submitted";
    await deps.persist(session);
    const result = await deps.rewindFiles(
      receipt.sourceNativeId,
      receipt.fileMessageId,
      { dryRun: false },
    );
    if (!result?.canRewind || result.skippedLinks > 0)
      throw new Error(
        result?.error ||
          "Claude could not restore every tracked file; inspect workspace recovery",
      );
    await deps.verifyFileRestore(receipt.filePlan, "expected");
    receipt.fileResult = result;
    receipt.status = "rewound";
    await deps.persist(session);
  }
  session.historyBranches ||= [];
  session.historyBranches.push({
    requestId,
    nativeId: receipt.sourceNativeId,
    turns: receipt.previousTurns,
  });
  session.nativeId = receipt.targetNativeId;
  session.started = retained.length > 0;
  if (!retained.length) session.nativeHistoryCwd = session.cwd;
  session.turns = retained;
  receipt.status = "completed";
  receipt.result = {
    threadId: session.id,
    nativeId: session.nativeId,
    removedTurns: receipt.previousTurns.length - retained.length,
    ...(restoreFiles
      ? { filesRestored: true, fileResult: receipt.fileResult }
      : {}),
  };
  delete receipt.sourceMessages;
  delete receipt.previousTurns;
  await deps.persist(session);
  return receipt.result;
}

// The Runtime workspace operation owns submission identity and unknown outcomes.
export async function forkAtTurn(source, params, deps) {
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
      ["inProgress", "uncertain"].includes(turn.status),
    ) ||
    (selected >= 0 && source.turns[selected].status !== "completed")
  )
    throw new Error("Select a completed Claude turn before branching");
  let targetNativeId;
  if (!source.started && !source.turns.length) {
    targetNativeId = (deps.randomUUID || randomUUID)();
  } else {
    const nativeId = source.nativeId || source.id;
    const dir = source.nativeHistoryCwd || source.cwd;
    const messages = await deps.getSessionMessages(nativeId, { dir });
    const positions = turnPositions(
      source.turns.slice(0, selected + 2),
      messages,
    );
    const end =
      selected < source.turns.length - 1
        ? positions[selected + 1][0]
        : messages.length;
    if (
      end < 1 ||
      positions.slice(0, retained.length).some((parts) => parts.at(-1) >= end)
    )
      throw new Error(
        "The exact Claude branch boundary is unavailable after compaction or history recovery",
      );
    preflightRetained(retained, messages);
    const fork = await deps.forkSession(nativeId, {
      dir,
      ...(params.lastTurnId != null
        ? { upToMessageId: messages[end - 1]?.uuid }
        : {}),
    });
    targetNativeId = fork?.sessionId;
    if (!targetNativeId || targetNativeId === nativeId)
      throw new Error("Claude returned an invalid fork receipt");
    const forkMessages = await deps.getSessionMessages(targetNativeId, { dir });
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
    for (const turn of retained) remapTurn(turn, messages, forkMessages);
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
