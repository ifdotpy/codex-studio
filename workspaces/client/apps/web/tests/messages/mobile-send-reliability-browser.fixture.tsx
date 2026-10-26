import React from "react";
import { createRoot } from "react-dom/client";
import { useOutbox } from "../../src/sync/send.ts";
export function mount() {
  const root = createRoot(document.body);
  root.render(
    React.createElement(function Harness() {
      window.mobileSendOutbox = useOutbox();
      return null;
    }),
  );
  return root;
}

const initialize = async () => {
  window.mobileSendSend = await import("../../src/sync/send.ts");
  window.mobileSendStorage = await (
    await import("../../src/sync/client.ts")
  ).syncDatabase();
  window.mobileSendFetchCount = 0;
  const original = window.fetch;
  window.fetch = (...args) => {
    window.mobileSendFetchCount++;
    return original(...args);
  };
  window.mobileSendRead = async (id) => {
    const doc = await window.mobileSendStorage.db.outbox.findOne(id).exec();
    return doc && JSON.parse(doc.getLatest().payload);
  };
  window.mobileSendStart = (body) => {
    window.mobileSendPersisted = false;
    window.mobileSendSettled = false;
    window.mobileSendResult = window.mobileSendSend
      .durableSend(body, [], () => {
        window.mobileSendPersisted = true;
        window.mobileSendPersistedRecord = window.mobileSendRead(body.id);
      })
      .then((result) => {
        window.mobileSendSettled = true;
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
    mobileSendSend: typeof import("../../src/sync/send.ts");
    mobileSendStorage: Awaited<
      ReturnType<typeof import("../../src/sync/client.ts").syncDatabase>
    >;
    mobileSendFetchCount: number;
    mobileSendRead: (
      id: string,
    ) => Promise<Omit<
      import("../../src/sync/send.ts").OutboxEntry,
      "id"
    > | null>;
    mobileSendStart: (
      body: Parameters<typeof import("../../src/sync/send.ts").durableSend>[0],
    ) => void;
    mobileSendPersisted: boolean;
    mobileSendSettled: boolean;
    mobileSendResult: ReturnType<
      typeof import("../../src/sync/send.ts").durableSend
    >;
    mobileSendPersistedRecord: ReturnType<Window["mobileSendRead"]>;
    mobileSendOutbox: ReturnType<typeof useOutbox>;
    mobileSendReactRoot: ReturnType<typeof createRoot>;
  }
}
