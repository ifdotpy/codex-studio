export async function mount() {
  window.preflightSend_send = await import("../../src/sync/send.ts");
  window.preflightSend_storage = await (
    await import("../../src/sync/client.ts")
  ).syncDatabase();
  window.preflightSend_read = read;
  window.preflightSend_start = (body, copies) => {
    window.preflightSend_result = Promise.all(
      Array.from({ length: copies }, () =>
        window.preflightSend_send.durableSend(body).catch((error: unknown) => {
          if (!(error instanceof Error)) throw error;
          return {
            error: error.message,
            code:
              "status" in error && typeof error.status === "number"
                ? error.status
                : undefined,
          };
        }),
      ),
    );
  };
}

declare global {
  interface Window {
    preflightSend_send: typeof import("../../src/sync/send.ts");
    preflightSend_storage: Awaited<
      ReturnType<typeof import("../../src/sync/client.ts").syncDatabase>
    >;
    preflightSend_read: typeof read;
    preflightSend_start: (
      body: import("../../src/apiTypes.ts").PostBody<"/api/messages">,
      copies: number,
    ) => void;
    preflightSend_result: Promise<
      Array<
        Partial<
          Awaited<
            ReturnType<typeof import("../../src/sync/send.ts").durableSend>
          >
        > & { error?: string | null | undefined; code?: number | undefined }
      >
    >;
  }
}
async function read(
  id: string,
): Promise<import("../../src/sync/send.ts").OutboxEntry> {
  const doc = await window.preflightSend_storage.db.outbox.findOne(id).exec();
  if (!doc) throw new Error("Missing outbox record");
  // The renderer writes and reads this outbox document.
  return JSON.parse(doc.getLatest().payload);
}
