import type { Agent } from "../../src/types.ts";
import { required } from "./helpers.ts";
declare global {
  interface Window {
    confirmedConcurrencyFixture: {
      refreshCount: number;
      refreshHold: boolean;
      refreshFailure: boolean;
      refreshRelease: () => void;
      setLead: (lead: Agent) => void;
    };
  }
}
import React, { useState } from "react";
import { createRoot } from "react-dom/client";
import { flushSync } from "react-dom";
import { MantineProvider } from "@mantine/core";
import "@mantine/core/styles.css";
import Control from "../../src/components/agents/SubagentConcurrencyControl.tsx";
window.confirmedConcurrencyFixture = {
  refreshCount: 0,
  refreshHold: true,
  refreshFailure: false,
  refreshRelease: () => {
    throw Error("Refresh has not started");
  },
  setLead: () => {
    throw Error("Fixture has not rendered");
  },
};
function Fixture() {
  const [lead, setLead] = useState<Agent>({
    id: "lead",
    accountKey: "default",
    name: "Lead",
    isLead: true,
    status: "idle",
    subagentConcurrencyVersion: 2,
    concurrency: 32,
    agentModeRevision: 0,
  });
  window.confirmedConcurrencyFixture.setLead = (value) =>
    flushSync(() => setLead(value));
  return (
    <MantineProvider>
      <Control
        compact
        lead={lead}
        stateDir="fixture-state"
        workspaceId="1234567890abcdef1234567890abcdef"
        refresh={async () => {
          window.confirmedConcurrencyFixture.refreshCount++;
          if (window.confirmedConcurrencyFixture.refreshHold)
            await new Promise<void>(
              (resolve) =>
                (window.confirmedConcurrencyFixture.refreshRelease = resolve),
            );
          if (window.confirmedConcurrencyFixture.refreshFailure)
            throw new Error("Credentials unavailable");
        }}
      />
    </MantineProvider>
  );
}
createRoot(required(document.getElementById("root"))).render(<Fixture />);
