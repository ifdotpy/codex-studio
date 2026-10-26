declare global {
  interface Window {
    syncTranscript: {
      stopTranscript: () => void;
      cursor: () => Promise<number | undefined>;
    };
  }
}
export const initializeTranscript = async () => {
  const client = await import("/src/sync/client.ts");
  window.syncTranscript = {
    stopTranscript: () => {},
    cursor: async () => undefined,
  };
  window.syncTranscript.stopTranscript = client.subscribeTranscriptProjection(
    "transcript:open",
    (payload) => {
      const main = document.querySelector("main");
      if (!main) throw new Error("Missing transcript fixture main");
      main.textContent =
        payload?.items.map((item) => item.text).join("\n") || "";
    },
    (error) => {
      if (error) throw error;
    },
  );
  window.syncTranscript.cursor = async () => {
    const { db } = await client.syncDatabase();
    return (await db.projections.findOne("transcript:open").exec())?.seq;
  };
};
