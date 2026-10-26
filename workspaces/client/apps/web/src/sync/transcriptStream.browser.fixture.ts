declare global {
  interface Window {
    stopTranscript: () => void;
    cursor: () => Promise<number | undefined>;
  }
}
export const initializeTranscript = async () => {
  const client = await import("/src/sync/client.ts");
  window.stopTranscript = client.subscribeTranscriptProjection(
    "transcript:open",
    (payload) => {
      document.querySelector("main")!.textContent =
        payload?.items.map((item) => item.text).join("\n") || "";
    },
    (error) => {
      if (error) throw error;
    },
  );
  window.cursor = async () => {
    const { db } = await client.syncDatabase();
    return (await db.projections.findOne("transcript:open").exec())?.seq;
  };
};
