import type { Snapshot } from "../../src/types.ts";
type Focus = React.ComponentProps<typeof BackgroundTasks>["initialFocus"];
declare global {
  interface Window {
    backgroundCurrentBrowserRenderFixture: (
      serialized: string,
      leadId?: string,
      focus?: Focus,
    ) => void;
    backgroundCurrentBrowserRefreshCalls: number;
    backgroundCurrentBrowserHoldRefresh: boolean;
    backgroundCurrentBrowserResolveRefresh: () => void;
    backgroundCurrentBrowserRejectRefresh: (reason: Error) => void;
    backgroundCurrentBrowserNotifications: string[];
  }
}
import { required } from "../teams/helpers.ts";

import React from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import "@mantine/core/styles.css";
import "../../src/style.css";
import BackgroundTasks from "../../src/components/shell/BackgroundTasks.tsx";
const root = createRoot(required(document.getElementById("root")));
window.backgroundCurrentBrowserRenderFixture = (
  serialized,
  leadId = "lead",
  initialFocus,
) => {
  // The test serialized this fixture snapshot from its typed owner.
  const data: Snapshot = JSON.parse(serialized);
  root.render(
    React.createElement(
      MantineProvider,
      { defaultColorScheme: "dark" },
      React.createElement(BackgroundTasks, {
        opened: true,
        close: () => {},
        data,
        leadId,
        initialFocus,
        openAgent: () => {},
        refresh: async () => {
          window.backgroundCurrentBrowserRefreshCalls =
            (window.backgroundCurrentBrowserRefreshCalls || 0) + 1;
          if (window.backgroundCurrentBrowserHoldRefresh)
            await new Promise<void>((resolve, reject) => {
              window.backgroundCurrentBrowserResolveRefresh = resolve;
              window.backgroundCurrentBrowserRejectRefresh = reject;
            });
        },
        notify: (error) =>
          (window.backgroundCurrentBrowserNotifications ??= []).push(error),
      }),
    ),
  );
};
