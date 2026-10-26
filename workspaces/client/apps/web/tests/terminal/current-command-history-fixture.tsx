import type { Message } from "../../src/types.ts";
import { required } from "../teams/helpers.ts";
export type HistorySnapshot = {
  label: string;
  items: Message[];
  enabled: boolean;
  currentTurn?: string | undefined;
};
declare global {
  interface Window {
    currentCommandHistoryApply: (snapshot: HistorySnapshot) => void;
  }
}

import React, { useState } from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import "@mantine/core/styles.css";
import TurnHistory from "../../src/components/conversation/transcript/TurnHistory.tsx";
function Fixture() {
  const [snapshot, setSnapshot] = useState<HistorySnapshot | null>(null);
  window.currentCommandHistoryApply = setSnapshot;
  return (
    <MantineProvider>
      <main data-case={snapshot?.label}>
        {snapshot && (
          <TurnHistory
            key={snapshot.label}
            items={snapshot.items}
            currentTurn={snapshot.currentTurn}
            enabled={snapshot.enabled}
            storageKey="fixture"
            renderMessage={(item) => <p data-message={item.id}>{item.text}</p>}
            onJump={() => {}}
          />
        )}
        <output hidden>{JSON.stringify(snapshot?.items)}</output>
      </main>
    </MantineProvider>
  );
}
createRoot(required(document.getElementById("root"))).render(<Fixture />);
