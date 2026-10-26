import type { Agent, Request } from "../../src/types.ts";
import { required } from "../teams/helpers.ts";

import React from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import "@mantine/core/styles.css";
import Requests from "../../src/components/questions/Requests.tsx";
const agent: Agent = {
  created: 1,
  updated: 1,
  source: "managed",
  id: "lead",
  rootId: "lead",
  name: "Lead",
  epoch: 0,
  accountKey: "default",
  turnId: "turn",
  inFlight: true,
};
const request: Request = {
  id: "request-159",
  agent: "lead",
  method: "item/commandExecution/requestApproval",
  params: {
    kind: "writeStdin",
    threadId: "thread-159",
    turnId: "turn-159",
    itemId: "exec-159",
    startedAtMs: 1750000000000,
    approvalId: "stdin-approval-159",
    reason:
      "Send input to an existing terminal to continue the reviewed command.",
  },
};
createRoot(required(document.getElementById("root"))).render(
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
