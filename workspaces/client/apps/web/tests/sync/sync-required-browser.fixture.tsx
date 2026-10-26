import { fixtureElement } from "./browser-helpers.ts";
import React from "react";
import { createRoot } from "react-dom/client";
const mount1 = async () => {
  const { useOutbox } = await import("../../src/sync/send.ts");
  const { useSyncedDrafts } = await import("../../src/sync/drafts.ts");
  const root = document.createElement("div");
  document.body.appendChild(root);
  createRoot(root).render(
    React.createElement(function Harness() {
      const outbox = useOutbox(),
        drafts = useSyncedDrafts();
      window.syncRequiredBrowserFixtureState = { outbox, drafts };
      return null;
    }),
  );
};
window.syncRequiredBrowserMount1 = mount1;
declare global {
  interface Window {
    syncRequiredBrowserMount1: typeof mount1;
  }
}
const mount2 = async () => {
  const { useSyncedDrafts } = await import("../../src/sync/drafts.ts");
  const root = createRoot(fixtureElement("app"));
  root.render(
    React.createElement(function Harness() {
      const drafts = useSyncedDrafts();
      window.syncRequiredBrowserFixtureState = { drafts };
      return null;
    }),
  );
};
window.syncRequiredBrowserMount2 = mount2;
declare global {
  interface Window {
    syncRequiredBrowserMount2: typeof mount2;
  }
}

declare global {
  interface Window {
    syncRequiredBrowserFixtureState: {
      drafts: ReturnType<
        typeof import("../../src/sync/drafts.ts").useSyncedDrafts
      >;
      outbox?: ReturnType<typeof import("../../src/sync/send.ts").useOutbox>;
    };
    syncRequiredBrowserFixtureRestoreDraftWrites: () => void;
  }
}
