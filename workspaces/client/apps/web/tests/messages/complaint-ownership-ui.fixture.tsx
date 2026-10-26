import { required } from "./browser-helpers.ts";
import { fixtureSnapshot } from "../fixtures/snapshot.ts";
import type { Complaint, Snapshot } from "../../src/types.ts";

import React from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import "@mantine/core/styles.css";
import UserMessages from "../../src/components/shell/messages/UserMessages.tsx";
const root = createRoot(required(document.getElementById("root")));
let records: Complaint[] = [],
  target: "user" | "lead" = "user";
const data: Snapshot = {
  ...fixtureSnapshot([
    { id: "lead", name: "Release lead" },
    { id: "worker", name: "Worker" },
  ]),
  stateDir: "ownership-contract",
};
const render = () =>
  root.render(
    React.createElement(
      MantineProvider,
      null,
      React.createElement(UserMessages, {
        data: { ...data, runtime: { ...data.runtime, complaints: records } },
        target,
        refresh: async () => {},
        notify: () => {},
      }),
    ),
  );
window.ownershipSetRecords = (value) => {
  records = value;
  render();
};
window.ownershipSetTarget = (value) => {
  target = value;
  render();
};

declare global {
  interface Window {
    ownershipSetRecords: (value: Complaint[]) => void;
    ownershipSetTarget: (value: "user" | "lead") => void;
    ownershipWrongMessage: boolean;
    ownershipObserver: MutationObserver;
  }
}
