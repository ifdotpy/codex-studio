import type { Agent } from "../../src/types.ts";
export type ContextRepairAgent = Omit<Agent, "capacityRetry"> & {
  capacityRetry?: Omit<NonNullable<Agent["capacityRetry"]>, "updatedAt"> | null;
};
export type ContextRepairSnapshot = {
  label: string;
  agent: ContextRepairAgent;
};
declare global {
  interface Window {
    applyContextRepairSnapshot: (snapshot: string) => void;
  }
}

import React, { useState } from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import "@mantine/core/styles.css";
import { NativeError } from "../../src/components/NativeNotice.tsx";
function Fixture() {
  const [snapshot, setSnapshot] = useState<ContextRepairSnapshot | null>(null);
  // The spec writes this snapshot; serialization avoids recursive Playwright type expansion.
  window.applyContextRepairSnapshot = (serialized) => {
    const snapshot: ContextRepairSnapshot = JSON.parse(serialized);
    setSnapshot(snapshot);
  };
  return (
    <MantineProvider>
      <main data-case={snapshot?.label}>
        {snapshot && (
          <NativeError
            // The base partial capacityRetry fixture omits updatedAt.
            agent={snapshot.agent as Agent}
          />
        )}
        <output>{JSON.stringify(snapshot?.agent)}</output>
      </main>
    </MantineProvider>
  );
}
createRoot(document.getElementById("root")!).render(<Fixture />);
