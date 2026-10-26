import type { Agent } from "../../src/types.ts";
export type ContextRepairSnapshot = { label: string; agent: Agent };
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
  // This fixture reads the snapshot serialized by its own spec.
  window.applyContextRepairSnapshot = (serialized) => {
    const snapshot: ContextRepairSnapshot = JSON.parse(serialized);
    setSnapshot(snapshot);
  };
  return (
    <MantineProvider>
      <main data-case={snapshot?.label}>
        {snapshot && <NativeError agent={snapshot.agent} />}
        <output>{JSON.stringify(snapshot?.agent)}</output>
      </main>
    </MantineProvider>
  );
}
const root = document.getElementById("root");
if (!root) throw new Error("Missing fixture root");
createRoot(root).render(<Fixture />);
