import React from "react";
import { createRoot } from "react-dom/client";
const mount1 = async () => {
  const { useSnapshot } = await import("../../src/hooks.ts");
  const send = await import("../../src/sync/send.ts");
  window.durableSend = send.durableSend;
  const node = document.createElement("div");
  document.body.appendChild(node);
  createRoot(node).render(
    React.createElement(function Harness() {
      window.syncRecoveryBrowserFixtureSnapshot = useSnapshot();
      window.second = useSnapshot();
      window.outbox = send.useOutbox();
      return null;
    }),
  );
};
window.syncRecoveryBrowserMount1 = mount1;
declare global {
  interface Window {
    syncRecoveryBrowserMount1: typeof mount1;
  }
}

declare global {
  interface Window {
    durableSend: typeof import("../../src/sync/send.ts").durableSend;
    syncRecoveryBrowserFixtureSnapshot: import("./browser-helpers.ts").FixtureSnapshot;
    second: import("./browser-helpers.ts").FixtureSnapshot;
    outbox: ReturnType<typeof import("../../src/sync/send.ts").useOutbox>;
  }
}
