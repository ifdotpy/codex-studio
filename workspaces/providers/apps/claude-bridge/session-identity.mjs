// @ts-check
import { randomUUID } from "node:crypto";
import { readHistoryIndex } from "./history-index.mjs";
import { HUMAN_INPUT } from "./input-origin.generated.mjs";
import { nativeUuid } from "./native-history.mjs";

/** @typedef {"init" | import("@anthropic-ai/claude-agent-sdk").SessionStartHookInput["source"]} TransitionSource */
/** @typedef {{inputId: string, turnId: string, attemptId: string}} NativeInputAttempt */
/** @typedef {{id?: string, delivery?: string, deliveryStatus?: string, nativeSubmissionAttemptId?: string}} NativeInputItem */
/** @typedef {{id?: string, clientUserMessageId?: string, startOutcome?: string, items?: NativeInputItem[], nativeSessionId?: string, nativeHistoryDetached?: boolean, fileCheckpointMessageId?: string, fileCheckpointSource?: unknown}} NativeTurn */
/** @typedef {{from: string, target?: string, source: TransitionSource | "input", status: "pending" | "unknown", inputId?: string, turnId?: string, attemptId?: string}} PendingTransition */
/** @typedef {{id: string, nativeId?: string, cwd: string, nativeHistoryCwd?: string, started?: boolean, pendingNativeSession?: PendingTransition, nativeSessionTransitionError?: string, nativeSessionTransitions?: {from: string, to: string, source: string}[], turns: NativeTurn[]}} NativeSession */

/** Stable input identity is reused for redelivery; an SDK submission attempt is
 * never reused. Both the receipt and guard retain this exact turn/attempt key.
 * @param {NativeTurn | undefined} turn @param {NativeInputItem | undefined} item
 * @returns {NativeInputAttempt | null}
 */
export function nativeInputAttempt(turn, item) {
  const inputId =
    item?.delivery === "steer"
      ? item.id
      : turn?.clientUserMessageId || turn?.id;
  return item &&
    turn?.id &&
    inputId &&
    nativeUuid(item.nativeSubmissionAttemptId)
    ? { inputId, turnId: turn.id, attemptId: item.nativeSubmissionAttemptId }
    : null;
}
/** @param {NativeTurn} turn @param {NativeInputItem} item @returns {NativeInputAttempt} */
export function newNativeInputAttempt(turn, item) {
  item.nativeSubmissionAttemptId = randomUUID();
  const attempt = nativeInputAttempt(turn, item);
  if (!attempt)
    throw new Error("A Claude input submission attempt identity is required");
  return attempt;
}
/** @param {Partial<NativeInputAttempt> | undefined} left @param {NativeInputAttempt | null | undefined} right */
function sameAttempt(left, right) {
  return (
    !!left &&
    !!right &&
    typeof left.inputId === "string" &&
    typeof left.turnId === "string" &&
    nativeUuid(left.attemptId) &&
    left.inputId === right.inputId &&
    left.turnId === right.turnId &&
    left.attemptId === right.attemptId
  );
}

/** Persisted native identity comes only from init or the typed SessionStart hook.
 * Conversation-reset ids are UI conversation identities, not resumable sessions.
 * @param {NativeSession} session
 * @param {string | undefined} nativeId
 * @param {"init" | import("@anthropic-ai/claude-agent-sdk").SessionStartHookInput["source"]} source
 * @param {NativeTurn | undefined} turn
 */
function transitionNativeSession(session, nativeId, source, turn) {
  // Some older SDK fixtures omit init identity. They supply no transition.
  if (nativeId === undefined) return;
  if (!nativeUuid(nativeId))
    throw new Error("Claude returned an invalid native session identity");
  const previous = session.nativeId || session.id;
  if (previous !== nativeId) {
    session.nativeSessionTransitions ||= [];
    session.nativeSessionTransitions.push({
      from: previous,
      to: nativeId,
      source,
    });
    session.nativeId = nativeId;
    session.nativeHistoryCwd = session.cwd;
    if (turn) turn.nativeHistoryDetached = true;
    // A checkpoint belongs to the old session and cannot restore this branch.
    for (const old of session.turns) {
      old.nativeSessionId ||= previous;
      delete old.fileCheckpointMessageId;
      delete old.fileCheckpointSource;
    }
  }
  session.nativeId = nativeId;
  session.started = true;
  if (turn) turn.nativeSessionId = nativeId;
}

/** An input that can clear/fork the native session must have a durable guard
 * in the existing pre-input save, before any part reaches the SDK.
 * @param {NativeSession} session
 * @param {{origin: string, blocks: {type: string, text?: string}[]}[]} groups
 * @param {NativeInputAttempt} attempt
 */
export function guardNativeSessionInput(session, groups, attempt) {
  if (
    groups.some(
      (group) =>
        group.origin === HUMAN_INPUT &&
        /^\s*\/(?:clear|reset|new|fork)(?:\s|$)/.test(
          group.blocks
            .filter((block) => block.type === "text")
            .map((block) => block.text || "")
            .join("\n"),
        ),
    )
  ) {
    if (
      session.pendingNativeSession?.source === "input" &&
      !sameAttempt(session.pendingNativeSession, attempt)
    )
      throw new Error("Another Claude native session transition is pending");
    session.pendingNativeSession = {
      from: session.nativeId || session.id,
      source: "input",
      status: "pending",
      ...attempt,
    };
  }
}

/** A confirmed unread attempt cannot retire a later redelivery's guard.
 * @param {NativeSession} session @param {NativeInputAttempt | null | undefined} attempt
 */
export function cancelNativeSessionInput(session, attempt) {
  const pending = session.pendingNativeSession;
  if (
    pending &&
    !pending.target &&
    pending.from === (session.nativeId || session.id) &&
    (pending.source === "input" || pending.source === "init") &&
    sameAttempt(pending, attempt)
  ) {
    delete session.pendingNativeSession;
    delete session.nativeSessionTransitionError;
    return true;
  }
  return false;
}

/** Guard the native process initialization before spawning it.
 * @param {NativeSession} session
 * @param {(session: NativeSession) => Promise<unknown>} persist
 * @param {NativeInputAttempt | null} [attempt]
 */
export async function guardNativeSessionStart(session, persist, attempt) {
  session.pendingNativeSession ||= {
    from: session.nativeId || session.id,
    source: "init",
    status: "pending",
    ...attempt,
  };
  await persist(session);
}

const UNKNOWN_NATIVE_TRANSITION =
  "Claude native session transition outcome unknown; resume is blocked until native history confirms it.";
/** @param {NativeSession} session @param {PendingTransition} intent */
function unknownTransition(session, intent) {
  session.pendingNativeSession = { ...intent, status: "unknown" };
  session.nativeSessionTransitionError = UNKNOWN_NATIVE_TRANSITION;
}

/** Save intent before mutating identity, then atomically save the transition.
 * A pre-input/start guard survives a crash before this first save can append.
 * @param {NativeSession} session
 * @param {string | undefined} nativeId
 * @param {TransitionSource} source
 * @param {NativeTurn | undefined} turn
 * @param {(session: NativeSession) => Promise<unknown>} persist
 */
export async function persistNativeSession(
  session,
  nativeId,
  source,
  turn,
  persist,
) {
  if (!nativeUuid(nativeId))
    throw new Error("Claude returned an invalid native session identity");
  const previous = session.nativeId || session.id;
  const pending = session.pendingNativeSession;
  if (nativeId !== previous) {
    session.pendingNativeSession = {
      from: previous,
      target: nativeId,
      source,
      status: "pending",
      ...(pending?.inputId ? { inputId: pending.inputId } : {}),
      ...(pending?.turnId ? { turnId: pending.turnId } : {}),
      ...(pending?.attemptId ? { attemptId: pending.attemptId } : {}),
    };
    try {
      await persist(session);
    } catch (error) {
      unknownTransition(session, session.pendingNativeSession);
      throw error;
    }
  }
  const intent = session.pendingNativeSession;
  transitionNativeSession(session, nativeId, source, turn);
  // An initial init for the old session does not settle a queued /clear.
  if (
    !(nativeId === previous && pending?.source === "input" && !pending.target)
  ) {
    delete session.pendingNativeSession;
    delete session.nativeSessionTransitionError;
  }
  try {
    await persist(session);
  } catch (error) {
    if (intent) session.pendingNativeSession = { ...intent, status: "unknown" };
    session.nativeSessionTransitionError = UNKNOWN_NATIVE_TRANSITION;
    throw error;
  }
}

/** Resolve only a target whose complete SDK history proves its identity.
 * A target-less intent cannot be guessed from project/session names.
 * @param {NativeSession} session
 * @param {import("./history-index.mjs").HistoryReadOptions} [options]
 */
export async function reconcileNativeSession(session, options) {
  const pending = session.pendingNativeSession;
  if (!pending) return false;
  // Only this exact attempt's durable receipt can retire its guard. An older
  // withdrawal of the same logical input says nothing about a consumed retry.
  const turn = session.turns.find((saved) => saved.id === pending.turnId);
  const item = turn?.items?.find((saved) =>
    sameAttempt(pending, nativeInputAttempt(turn, saved)),
  );
  const attempt = nativeInputAttempt(turn, item);
  if (
    !pending.target &&
    item &&
    turn &&
    ((turn.startOutcome === "not_applied" && item === turn.items?.[0]) ||
      ["not_applied", "cancelled"].includes(item.deliveryStatus || ""))
  ) {
    if (cancelNativeSessionInput(session, attempt)) return true;
  }
  const valid =
    nativeUuid(pending.from) &&
    pending.from === (session.nativeId || session.id) &&
    ["init", "input", "startup", "resume", "clear", "compact", "fork"].includes(
      pending.source,
    ) &&
    ["pending", "unknown"].includes(pending.status) &&
    (pending.inputId === undefined
      ? pending.turnId === undefined && pending.attemptId === undefined
      : typeof pending.inputId === "string" &&
        typeof pending.turnId === "string" &&
        nativeUuid(pending.attemptId));
  if (!valid) {
    unknownTransition(session, {
      from: session.nativeId || session.id,
      source: "init",
      status: "unknown",
    });
    return false;
  }
  if (
    nativeUuid(pending.target) &&
    pending.target !== pending.from &&
    pending.source !== "input"
  ) {
    const index = await readHistoryIndex(
      { ...session, nativeId: pending.target, nativeHistoryCwd: session.cwd },
      options,
    );
    if (index) {
      transitionNativeSession(
        session,
        pending.target,
        pending.source,
        undefined,
      );
      delete session.pendingNativeSession;
      delete session.nativeSessionTransitionError;
      return true;
    }
  }
  unknownTransition(session, pending);
  return false;
}

/** @param {NativeSession} session */
export function assertNativeSessionReady(session) {
  if (session.pendingNativeSession?.status === "unknown")
    throw Object.assign(
      new Error(
        session.nativeSessionTransitionError ||
          "Claude native session transition outcome unknown",
      ),
      {
        data: { turnStartOutcome: "unknown", nativeSessionOutcome: "unknown" },
      },
    );
}

/** @param {NativeSession} session
 * @param {() => NativeTurn | undefined} currentTurn
 * @param {(session: NativeSession) => Promise<unknown>} persist
 * @returns {import("@anthropic-ai/claude-agent-sdk").HookCallback}
 */
export function sessionStartHook(session, currentTurn, persist) {
  return async (input) => {
    if (
      input.hook_event_name === "SessionStart" &&
      (input.source === "clear" ||
        input.source === "fork" ||
        input.session_id !== (session.nativeId || session.id))
    ) {
      await persistNativeSession(
        session,
        input.session_id,
        input.source,
        currentTurn(),
        persist,
      );
    }
    return {};
  };
}
