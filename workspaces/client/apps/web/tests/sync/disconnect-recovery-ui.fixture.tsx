import type { Agent } from "../../src/types.ts";
import { original } from "./disconnect-recovery-ui.data.ts";
import { fixtureElement } from "./browser-helpers.ts";

import React, { useState } from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import "@mantine/core/styles.css";
import { NativeError } from "../../src/components/NativeNotice.tsx";
import { setToken } from "../../src/api.ts";
setToken("fixture-token");
window.disconnectRecoveryUiFixtureRefreshes = [];
function Harness() {
  const [agent, setAgent] = useState<Agent>(() => readAgent());
  window.disconnectRecoveryUiFixtureSetAgent = setAgent;
  return (
    <MantineProvider>
      <div data-agent={agent.id}>
        <NativeError
          agent={agent}
          refresh={async () => {
            window.disconnectRecoveryUiFixtureRefreshes.push(agent.id);
          }}
        />
        <p data-history>Previously recorded conversation remains available.</p>
      </div>
    </MantineProvider>
  );
}
createRoot(fixtureElement("root")).render(<Harness />);

declare global {
  interface Window {
    disconnectRecoveryUiFixtureRefreshes: string[];
    disconnectRecoveryUiFixtureSetAgent: import("react").Dispatch<
      import("react").SetStateAction<import("../../src/types.ts").Agent>
    >;
  }
}

// This fixture restores only the agent document it wrote itself.
function readAgent(): Agent {
  return (
    (JSON.parse(
      localStorage.getItem("fixture-agent") || "null",
    ) as Agent | null) ?? original
  );
}
