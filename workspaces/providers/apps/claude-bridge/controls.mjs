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

function nativeBoundary(turn) {
  return turn.nativeMessageId || turn.id;
}
function comparable(message) {
  return JSON.stringify({ type: message.type, message: message.message });
}

export function rollbackBoundary(turns, messages, turnId) {
  const index = turns.findIndex((turn) => turn.id === turnId);
  if (index < 0) throw new Error("The selected Claude turn does not exist");
  const positions = turns
    .slice(0, index + 1)
    .map((turn) =>
      messages.findIndex(
        (message) =>
          message.type === "user" &&
          !message.parent_tool_use_id &&
          message.uuid === nativeBoundary(turn),
      ),
    );
  if (
    positions.some(
      (position, i) => position < 0 || (i > 0 && position <= positions[i - 1]),
    )
  )
    throw new Error(
      "The exact Claude turn boundary is unavailable after compaction or history recovery",
    );
  const end = positions[index];
  return { index, end, upToMessageId: index ? messages[end - 1]?.uuid : null };
}

// Caller must serialize session mutations and close its idle SDK query first.
// persist must atomically save the whole descriptor. Never retry an unknown fork.
export async function rollbackSession(session, request, deps) {
  const { requestId, turnId } = request;
  if (typeof requestId !== "string" || !requestId.trim())
    throw new Error("A rollback request identity is required");
  if (typeof turnId !== "string" || !turnId)
    throw new Error("A Claude turn identity is required");
  session.controlRequests ||= {};
  let receipt = Object.hasOwn(session.controlRequests, requestId)
    ? session.controlRequests[requestId]
    : undefined;
  if (receipt && (receipt.kind !== "rollback" || receipt.turnId !== turnId))
    throw new Error("This request identity has different content");
  if (receipt?.status === "completed") {
    await deps.persist(session);
    return receipt.result;
  }
  if (receipt?.status === "submitted")
    throw new Error(
      "The Claude fork outcome is unknown; this request cannot repeat the fork",
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
    receipt = {
      kind: "rollback",
      turnId,
      status: "submitted",
      sourceNativeId,
      createdAt: new Date().toISOString(),
      retainedCount: boundary.index,
      sourceMessages: messages.slice(0, boundary.end),
      previousTurns: structuredClone(session.turns),
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
  if (receipt.status !== "forked")
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
    for (const turn of retained) {
      const index = source.findIndex(
        (message) => message.uuid === nativeBoundary(turn),
      );
      if (index < 0 || !forkMessages[index]?.uuid)
        throw new Error("Claude fork lost a retained turn boundary");
      turn.nativeMessageId = forkMessages[index].uuid;
    }
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
  };
  delete receipt.sourceMessages;
  delete receipt.previousTurns;
  await deps.persist(session);
  return receipt.result;
}

// The Runtime workspace operation owns submission identity and unknown outcomes.
export async function forkAtTurn(source, params, deps) {
  if (source.turns.some((turn) => turn.status === "inProgress"))
    throw new Error("Pause Claude before branching");
  const selected =
    params.lastTurnId == null
      ? source.turns.length - 1
      : source.turns.findIndex((turn) => turn.id === params.lastTurnId);
  if (params.lastTurnId != null && selected < 0)
    throw new Error("The selected Claude turn does not exist");
  const retained = structuredClone(source.turns.slice(0, selected + 1));
  let targetNativeId;
  if (!source.started && !source.turns.length) {
    targetNativeId = (deps.randomUUID || randomUUID)();
  } else {
    const nativeId = source.nativeId || source.id;
    const dir = source.nativeHistoryCwd || source.cwd;
    const messages = await deps.getSessionMessages(nativeId, { dir });
    let end = messages.length;
    if (selected < source.turns.length - 1) {
      end = messages.findIndex(
        (message) =>
          message.uuid === nativeBoundary(source.turns[selected + 1]),
      );
      if (end < 1)
        throw new Error(
          "The exact Claude branch boundary is unavailable after compaction or history recovery",
        );
    }
    const positions = retained.map((turn) =>
      messages.findIndex(
        (message) =>
          message.type === "user" &&
          !message.parent_tool_use_id &&
          message.uuid === nativeBoundary(turn),
      ),
    );
    if (
      positions.some(
        (position, i) =>
          position < 0 ||
          position >= end ||
          (i > 0 && position <= positions[i - 1]),
      )
    )
      throw new Error(
        "The exact Claude branch boundary is unavailable after compaction or history recovery",
      );
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
    for (let i = 0; i < retained.length; i++)
      retained[i].nativeMessageId = forkMessages[positions[i]].uuid;
  }
  const { controlRequests, historyBranches, ...base } = source;
  const { threadId, lastTurnId, ...settings } = params;
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
