// Native SDK checkpoints cover edits made through Claude's file tools.
export function fileCheckpointOptions(options) {
  return {
    ...options,
    enableFileCheckpointing: true,
    env: { ...options.env, CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING: "true" },
  };
}

export function checkpointSourceMatches(session, turn) {
  return (
    turn.fileCheckpointSource?.nativeId === (session.nativeId || session.id) &&
    turn.fileCheckpointSource?.cwd === session.cwd
  );
}

export function fileRestoreReason(session, turn, active) {
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
  query,
  options,
  nativeId,
  messageId,
  rewindOptions,
) {
  let release;
  const held = new Promise((resolve) => (release = resolve));
  // oxlint-disable-next-line require-yield
  async function* prompt() {
    await held;
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
