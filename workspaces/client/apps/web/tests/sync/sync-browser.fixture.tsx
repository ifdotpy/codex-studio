import React from "react";
import { createRoot } from "react-dom/client";
const mount1 = async () => {
  const { useOutbox } = await import("../../src/sync/send.ts");
  const root = document.createElement("div");
  document.body.appendChild(root);
  createRoot(root).render(
    React.createElement(function Harness() {
      useOutbox();
      return null;
    }),
  );
};
window.syncBrowserMount1 = mount1;
declare global {
  interface Window {
    syncBrowserMount1: typeof mount1;
  }
}

declare global {
  interface Window {
    entityStreams: {
      source: EventSource;
      resources: import("../playwright.ts").ResourceRef[];
    }[];
    syncBrowserFixtureValues: (string | null | undefined)[];
    stopSync: () => void;
    failure?: string | undefined;
    syncStopDrafts: Awaited<
      ReturnType<
        typeof import("../../src/sync/client.ts").startDraftReplication
      >
    >;
  }
}
