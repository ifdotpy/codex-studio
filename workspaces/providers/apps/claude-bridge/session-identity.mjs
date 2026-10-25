// @ts-check
import { nativeUuid } from "./native-history.mjs";

/** @typedef {{ id: string, nativeId?: string, cwd: string, nativeHistoryCwd?: string, started?: boolean, nativeSessionTransitions?: {from: string, to: string, source: string}[], turns: {nativeSessionId?: string, fileCheckpointMessageId?: string, fileCheckpointSource?: unknown, nativeHistoryDetached?: boolean}[] }} NativeSession */
/** @typedef {{nativeSessionId?: string, nativeHistoryDetached?: boolean}} NativeTurn */

/** Persisted native identity comes only from init or the typed SessionStart hook.
 * Conversation-reset ids are UI conversation identities, not resumable sessions.
 * @param {NativeSession} session
 * @param {string | undefined} nativeId
 * @param {"init" | import("@anthropic-ai/claude-agent-sdk").SessionStartHookInput["source"]} source
 * @param {NativeTurn | undefined} turn
 */
export function transitionNativeSession(session, nativeId, source, turn) {
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

/** @param {NativeSession} session
 * @param {() => NativeTurn | undefined} currentTurn
 * @param {(session: NativeSession) => Promise<unknown>} persist
 * @returns {import("@anthropic-ai/claude-agent-sdk").HookCallback}
 */
export function sessionStartHook(session, currentTurn, persist) {
  return async (input) => {
    if (
      input.hook_event_name === "SessionStart" &&
      (input.source === "clear" || input.source === "fork")
    ) {
      transitionNativeSession(
        session,
        input.session_id,
        input.source,
        currentTurn(),
      );
      await persist(session);
    }
    return {};
  };
}
