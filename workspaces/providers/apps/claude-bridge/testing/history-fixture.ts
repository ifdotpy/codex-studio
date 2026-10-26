import type {
  SessionStoreEntry,
  SessionStore,
} from "@anthropic-ai/claude-agent-sdk";
// SDK-shaped reads for in-memory filesystem fault injection. Installed SDK
// coverage lives in history-index.test.ts and cli-history-witness.ts.
export function fixtureSessionApi(
  rows: SessionStoreEntry[],
  {
    sessionId,
    cwd,
    lastModified = 3,
  }: { sessionId: string; cwd: string; lastModified?: number },
) {
  return {
    getSessionInfo: async () => ({
      sessionId,
      cwd,
      summary: "fixture",
      lastModified,
      fileSize: Buffer.byteLength(
        rows.map((row) => JSON.stringify(row) + "\n").join(""),
      ),
    }),
    importSessionToStore: async (_id: string, store: SessionStore) =>
      store.append(
        { sessionId, projectKey: cwd.replace(/[^a-zA-Z0-9]/g, "-") },
        rows,
      ),
    getSessionMessages: async () =>
      rows
        .filter(
          (row) =>
            ["user", "assistant"].includes(row.type) &&
            !row.isSidechain &&
            !row.isMeta,
        )
        .map((row) => ({
          type: row.type,
          uuid: row.uuid,
          session_id: row.sessionId,
          message: row.message,
          parent_tool_use_id: null,
          parent_agent_id: null,
        })),
  };
}
