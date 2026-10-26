import type { Complaint, Snapshot } from "../../src/types.ts";

import React from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import "@mantine/core/styles.css";
import UserMessages from "../../src/components/shell/messages/UserMessages.tsx";
const root = createRoot(document.getElementById("root")!);
let records: Complaint[] = [],
  target: "user" | "lead" = "user";
const render = () => {
  // This one-off fixture omits chats/edges and runtime collections unrelated to complaint ownership.
  const data = {
    stateDir: "ownership-contract",
    token: "fixture",
    threads: [
      { id: "lead", name: "Release lead" },
      { id: "worker", name: "Worker" },
    ],
    runtime: { complaints: records },
  } as Snapshot;
  return root.render(
    React.createElement(
      MantineProvider,
      null,
      React.createElement(UserMessages, {
        data,
        target,
        refresh: async () => {},
        notify: () => {},
      }),
    ),
  );
};
window.setRecords = (value) => {
  records = value;
  render();
};
window.setTarget = (value) => {
  target = value;
  render();
};

declare global {
  interface Window {
    setRecords: (value: Complaint[]) => void;
    setTarget: (value: "user" | "lead") => void;
    wrongMessage: boolean;
    observer: MutationObserver;
  }
}
