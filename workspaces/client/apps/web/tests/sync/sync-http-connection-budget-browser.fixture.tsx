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
window.syncHttpConnectionBudgetBrowserFixturePrefetchTranscript =
  prefetchTranscript;
window.syncHttpConnectionBudgetBrowserFixtureDurableSend = durableSend;
window.syncHttpConnectionBudgetBrowserFixtureMount = (selected) =>
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
    syncHttpConnectionBudgetBrowserFixturePrefetchTranscript: typeof import("../../src/sync/client.ts").prefetchTranscript;
    syncHttpConnectionBudgetBrowserFixtureDurableSend: typeof import("../../src/sync/send.ts").durableSend;
    syncHttpConnectionBudgetBrowserFixtureSnapshot: ReturnType<
      typeof import("../../src/hooks.ts").useSnapshot
    >;
    syncHttpConnectionBudgetBrowserFixtureMount: (selected: string) => void;
    syncHttpConnectionBudgetBrowserFixtureRefresh: Promise<unknown>;
    syncHttpConnectionBudgetBrowserFixtureWarm: Promise<unknown>;
  }
}
