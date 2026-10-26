// SDK-shaped reads for in-memory filesystem fault injection. Installed SDK
// coverage lives in history-index.test.mjs and cli-history-witness.mjs.
export function fixtureSessionApi(rows, { sessionId, cwd, lastModified = 3 }) {
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
    importSessionToStore: async (_id, store) =>
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
