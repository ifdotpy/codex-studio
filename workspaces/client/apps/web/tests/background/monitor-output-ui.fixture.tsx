import { fixtureSnapshot } from "../fixtures/snapshot.ts";
declare global {
  interface Window {
    monitorOutputUiNotices: string[];
  }
}
import { required } from "../teams/helpers.ts";

import React from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import "@mantine/core/styles.css";
import "../../src/style.css";
import BackgroundTasks from "../../src/components/shell/BackgroundTasks.tsx";
const data = fixtureSnapshot([
  { id: "lead", rootId: "lead", name: "Lead", epoch: 2, accountKey: "default" },
]);
createRoot(required(document.getElementById("root"))).render(
  React.createElement(
    MantineProvider,
    { defaultColorScheme: "dark" },
    React.createElement(BackgroundTasks, {
      opened: true,
      close: () => {},
      data,
      leadId: "lead",
      initialFocus: { id: "monitor", leadId: "lead", requestId: "focus" },
      openAgent: () => {},
      refresh: async () => {},
      notify: (message) => (window.monitorOutputUiNotices ??= []).push(message),
    }),
  ),
);
