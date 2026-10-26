declare global {
  interface Window {
    syncErrors: string[];
    stopConnection: () => void;
    stopProjection: () => void;
  }
}
export const initializeDelivery = async () => {
  const client = await import("/src/sync/client.ts");
  window.snapshot = null;
  window.syncErrors = [];
  window.connection = "closed";
  window.stopConnection = client.watchResourceConnection(
    (state) => (window.connection = state),
  );
  window.stopProjection = client.subscribeStateProjection(
    (snapshot) => (window.snapshot = snapshot),
    (error) => {
      if (error) window.syncErrors.push(String(error));
    },
  );
  window.checkpoint = async () => {
    const { db } = await client.syncDatabase();
    return (await db.projections.findOne("state:entities:checkpoint").exec())
      ?.seq;
  };
};
