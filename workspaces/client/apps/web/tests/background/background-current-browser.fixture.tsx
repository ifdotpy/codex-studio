import type { Snapshot } from "../../src/types.ts";
type Focus = React.ComponentProps<typeof BackgroundTasks>["initialFocus"];
declare global {
  interface Window {
    renderFixture: (data: Snapshot, leadId?: string, focus?: Focus) => void;
    refreshCalls: number;
    holdRefresh: boolean;
    resolveRefresh: () => void;
    rejectRefresh: (reason: Error) => void;
    notifications: string[];
  }
}

import React from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import "@mantine/core/styles.css";
import "../../src/style.css";
import BackgroundTasks from "../../src/components/shell/BackgroundTasks.tsx";
const root = createRoot(document.getElementById("root")!);
window.renderFixture = (data, leadId = "lead", initialFocus) => {
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
          window.refreshCalls = (window.refreshCalls || 0) + 1;
          if (window.holdRefresh)
            await new Promise<void>((resolve, reject) => {
              window.resolveRefresh = resolve;
              window.rejectRefresh = reject;
            });
        },
        notify: (error) => (window.notifications ??= []).push(error),
      }),
    ),
  );
};
