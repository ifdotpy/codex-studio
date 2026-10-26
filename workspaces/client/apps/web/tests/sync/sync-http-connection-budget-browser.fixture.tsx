import { fixtureElement } from "./browser-helpers.ts";

import React, { useEffect } from "react";
import { createRoot } from "react-dom/client";
import { useSnapshot } from "../../src/hooks.ts";
import { useSyncedDrafts } from "../../src/sync/drafts.ts";
import {
  subscribeTranscriptProjection,
  watchResourceChanges,
  prefetchTranscript,
} from "../../src/sync/client.ts";
import { durableSend } from "../../src/sync/send.ts";
window.prefetchTranscript = prefetchTranscript;
window.durableSend = durableSend;
window.mount = (selected) =>
  createRoot(fixtureElement("app")).render(
    React.createElement(function Fixture() {
      window.syncHttpConnectionBudgetBrowserFixtureSnapshot = useSnapshot();
      useSyncedDrafts();
      useEffect(
        () =>
          subscribeTranscriptProjection(
            `transcript:${selected}`,
            () => {},
            () => {},
          ),
        [],
      );
      useEffect(
        () =>
          window.syncHttpConnectionBudgetBrowserFixtureSnapshot.workspaceId
            ? watchResourceChanges(
                { kind: "transcript", agentId: selected },
                () => {},
              )
            : undefined,
        [window.syncHttpConnectionBudgetBrowserFixtureSnapshot.workspaceId],
      );
      return React.createElement(
        "div",
        { id: "error" },
        window.syncHttpConnectionBudgetBrowserFixtureSnapshot.error,
      );
    }),
  );

declare global {
  interface Window {
    prefetchTranscript: typeof import("../../src/sync/client.ts").prefetchTranscript;
    durableSend: typeof import("../../src/sync/send.ts").durableSend;
    syncHttpConnectionBudgetBrowserFixtureSnapshot: import("./browser-helpers.ts").FixtureSnapshot;
    mount: (selected: string) => void;
    refresh: Promise<unknown>;
    warm: Promise<unknown>;
  }
}
