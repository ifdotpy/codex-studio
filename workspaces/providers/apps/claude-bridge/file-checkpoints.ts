import type { Options } from "@anthropic-ai/claude-agent-sdk";
import type { Session, Turn } from "./session-state.ts";
// Native SDK checkpoints cover edits made through Claude's file tools.
export function fileCheckpointOptions<T extends Pick<Options, "env">>(
  options: T,
) {
  return {
    ...options,
    enableFileCheckpointing: true,
    env: { ...options.env, CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING: "true" },
  };
}

export function checkpointSourceMatches(
  session: Pick<Session, "id" | "nativeId" | "cwd">,
  turn: Pick<Turn, "fileCheckpointSource">,
) {
  return (
    turn.fileCheckpointSource?.nativeId === (session.nativeId || session.id) &&
    turn.fileCheckpointSource?.cwd === session.cwd
  );
}

export function fileRestoreReason(
  session: Pick<
    Session,
    "id" | "nativeId" | "cwd" | "turns" | "controlRequests"
  >,
  turn: Pick<Turn, "fileCheckpointSource" | "fileCheckpointMessageId">,
  active?: { turn?: unknown; tasks?: { size: number } },
) {
  if (
    active?.turn ||
    active?.tasks?.size ||
    session.turns.some((t) => t.status === "inProgress")
  )
    return "Pause Claude before restoring files.";
  if (
    Object.values(session.controlRequests || {}).some(
      (r) => r.status !== "completed",
    )
  )
    return "A Claude history operation needs recovery.";
  if (!turn.fileCheckpointMessageId)
    return "This message has no SDK file checkpoint. Select a message from a new turn.";
  if (!checkpointSourceMatches(session, turn))
    return "The file checkpoint belongs to another session or workspace.";
  return null;
}
