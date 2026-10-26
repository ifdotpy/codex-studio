import type { Agent } from "../../src/types.ts";
import { fixtureRoot } from "./usage-fixture.ts";

import React from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import "@mantine/core/styles.css";
import { NativeError } from "../../src/components/NativeNotice.tsx";
const dueAt = Number(new URLSearchParams(location.search).get("dueAt"));
const resume = {
  id: "resume-1",
  cause: "usage_limit",
  reason: null,
  status: "scheduled",
  dueAt: dueAt,
  plannedAt: dueAt,
  updatedAt: 1,
};
const authResume = {
  id: "auth-resume-1",
  status: "scheduled",
  cause: "auth",
  plannedAt: null,
  reason: null,
  dueAt: dueAt,
  updatedAt: 1,
};
const agent: Agent = {
  id: "chat-1",
  rootId: "chat-1",
  accountKey: "work",
  threadId: "thread-1",
  model: "gpt-6-astra",
  error: {
    codexErrorInfo: "usageLimitExceeded",
    message: "Usage limit reached",
  },
  nativeLimitErrorAt: dueAt - 60,
  status: "failed",
  isLead: true,
  usageResume: resume,
};
const authAgent: Agent = {
  ...agent,
  id: "chat-auth",
  error: { codexErrorInfo: "unauthorized", message: "Sign in again" },
  usageResume: authResume,
};
createRoot(fixtureRoot()).render(
  <MantineProvider>
    <>
      <NativeError
        agent={agent}
        limits={{
          accountKey: "work",
          at: dueAt - 30,
          data: {
            rateLimits: { primary: { usedPercent: 100, resetsAt: dueAt } },
          },
        }}
      />
      <NativeError agent={authAgent} />
    </>
  </MantineProvider>,
);
