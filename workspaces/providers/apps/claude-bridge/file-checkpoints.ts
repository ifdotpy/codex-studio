import type {
  Options,
  Query,
  SDKUserMessage,
  query as sdkQuery,
} from "@anthropic-ai/claude-agent-sdk";
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

// No prompt enters this query. It resumes only to call the SDK control method.
export async function rewindSessionFiles(
  query: (
    ...args: Parameters<typeof sdkQuery>
  ) => Pick<Query, "rewindFiles" | "close">,
  options: Options,
  nativeId: string,
  messageId: string,
  rewindOptions: Parameters<Query["rewindFiles"]>[1],
) {
  const { promise: held, resolve: release } = Promise.withResolvers<void>();
  async function* prompt(): AsyncGenerator<SDKUserMessage> {
    await held;
    yield* [];
  }
  const abortController = new AbortController();
  const timer = setTimeout(() => abortController.abort(), 20_000);
  let q;
  try {
    q = query({
      prompt: prompt(),
      options: fileCheckpointOptions({
        ...options,
        resume: nativeId,
        abortController,
        settings: { disableAllHooks: true },
        mcpServers: {},
        strictMcpConfig: true,
        tools: [],
      }),
    });
    return await q.rewindFiles(messageId, rewindOptions);
  } finally {
    clearTimeout(timer);
    release();
    q?.close();
  }
}
