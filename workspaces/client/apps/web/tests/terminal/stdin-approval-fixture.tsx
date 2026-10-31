import type { Agent } from "../../src/types.ts";
import { approval } from "../../src/approvalFixtures.ts";

import React from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import "@mantine/core/styles.css";
import Requests from "../../src/components/questions/Requests.tsx";
const agent: Agent = {
  id: "lead",
  rootId: "lead",
  name: "Lead",
  epoch: 0,
  accountKey: "default",
  turnId: "turn",
  inFlight: true,
};
const request = approval({
  id: "request-159",
  agent: "lead",
  turn: "turn-159",
  request: {
    kind: "exec",
    command: "",
    cwd: "",
    reason:
      "Send input to an existing terminal to continue the reviewed command.",
  },
  params: {
    kind: "writeStdin",
    threadId: "thread-159",
    itemId: "exec-159",
    startedAtMs: 1750000000000,
    approvalId: "stdin-approval-159",
  },
});
createRoot(document.getElementById("root")!).render(
  React.createElement(
    MantineProvider,
    null,
    React.createElement(Requests, {
      requests: [request],
      allRequests: [request],
      scope: "fixture",
      agents: [agent],
      refresh: async () => {},
      notify: () => {},
    }),
  ),
);
