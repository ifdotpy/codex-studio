import React from "react";
import { createRoot } from "react-dom/client";
import { useOutbox } from "../../src/sync/send.ts";
export function mount() {
  const root = createRoot(document.body);
  root.render(
    React.createElement(function Harness() {
      window.outbox = useOutbox();
      return null;
    }),
  );
  return root;
}

const initialize = async () => {
  window.send = await import("../../src/sync/send.ts");
  window.storage = await (
    await import("../../src/sync/client.ts")
  ).syncDatabase();
  window.fetchCount = 0;
  const original = window.fetch;
  window.fetch = (...args) => {
    window.fetchCount++;
    return original(...args);
  };
  window.read = async (id) => {
    const doc = await window.storage.db.outbox.findOne(id).exec();
    return doc && JSON.parse(doc.getLatest().payload);
  };
  window.mobileSendStart = (body) => {
    window.persisted = false;
    window.settled = false;
    window.mobileSendResult = window.send
      .durableSend(body, [], () => {
        window.persisted = true;
        window.persistedRecord = window.read(body.id);
      })
      .then((result) => {
        window.settled = true;
        return result;
      });
  };
};
window.mobileSendInitialize = initialize;

window.mobileSendMount = mount;

declare global {
  interface Window {
    mobileSendInitialize: typeof initialize;
    mobileSendMount: typeof mount;
    send: typeof import("../../src/sync/send.ts");
    storage: Awaited<
      ReturnType<typeof import("../../src/sync/client.ts").syncDatabase>
    >;
    fetchCount: number;
    read: (
      id: string,
    ) => Promise<Omit<
      import("../../src/sync/send.ts").OutboxEntry,
      "id"
    > | null>;
    mobileSendStart: (
      body: Parameters<typeof import("../../src/sync/send.ts").durableSend>[0],
    ) => void;
    persisted: boolean;
    settled: boolean;
    mobileSendResult: ReturnType<
      typeof import("../../src/sync/send.ts").durableSend
    >;
    persistedRecord: ReturnType<Window["read"]>;
    outbox: ReturnType<typeof useOutbox>;
    reactRoot: ReturnType<typeof createRoot>;
  }
}
