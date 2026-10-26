import type { ComponentProps } from "react";
export type BrowserAccessSnapshot = ComponentProps<typeof BrowserAccessNotice>;
declare global {
  interface Window {
    applyBrowserAccessSnapshot: (snapshot: BrowserAccessSnapshot) => void;
  }
}

import React, { useState } from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import "@mantine/core/styles.css";
import BrowserAccessNotice from "../../src/components/BrowserAccessNotice.tsx";
function Fixture() {
  const [snapshot, setSnapshot] = useState<BrowserAccessSnapshot>({
    accountKey: "default",
    active: true,
  });
  window.applyBrowserAccessSnapshot = setSnapshot;
  return (
    <MantineProvider>
      <main data-account={snapshot.accountKey}>
        <h2>Conversation</h2>
        <BrowserAccessNotice {...snapshot} />
      </main>
    </MantineProvider>
  );
}
const root = document.getElementById("root");
if (!root) throw new Error("Missing fixture root");
createRoot(root).render(<Fixture />);
