import type { components } from "../../src/generated/api.ts";
import { emitRuntimeChange } from "./native-runtime-resource-events.fixture.ts";
import { fixtureAccount } from "./fixture-helpers.ts";
import type { ResourceRef } from "../../src/sync/resourceEvents.ts";

declare global {
  interface Window {
    setRuntimeOpen: (opened: boolean) => void;
    setWarningAccount: (key: string) => void;
    emitRuntimeChange: (kind: ResourceRef["kind"]) => void;
    delayedRuntimeSignal: AbortSignal;
    finishOldRuntimeRead: ((response: Response) => void) | null;
  }
}
window.emitRuntimeChange = emitRuntimeChange;
const fixtureRoot = document.getElementById("root");
import React, { useState } from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider, Modal, Button } from "@mantine/core";
import "@mantine/core/styles.css";
import NativeRuntimeStatus from "../../src/components/NativeRuntimeStatus.tsx";
import Accounts from "../../src/components/Accounts.tsx";
import "../../src/studio-theme.css";
import "../../src/appearance.css";
import ConversationWarnings from "../../src/components/ConversationWarnings.tsx";
import { StudioErrorsProvider } from "../../src/components/StudioErrors.tsx";
const accounts = [
  { id: "default", label: "Personal", status: "ready" as const },
  {
    id: "work",
    label: "Work with a very long account label for narrow screens",
    status: "ready" as const,
  },
  {
    id: "claude",
    label: "Claude profile",
    provider: "claude" as const,
    status: "ready" as const,
  },
].map((account) => fixtureAccount(account));
// Partial fixture view: these notices omit transport-only at and connectionId.
const notices = [
  {
    id: "codex-warning",
    accountKey: "default",
    message: "Codex account version warning",
  },
  {
    id: "claude-warning",
    accountKey: "claude",
    message: "Claude account version warning",
  },
] as components["schemas"]["NativeNoticeRecord"][];
// Partial fixture view: base data omits archives/groups/logins/support flags; refresh returns no data.
const accountsState: Partial<
  Record<keyof React.ComponentProps<typeof Accounts>["state"], unknown>
> = {
  data: { accounts, defaultAccountKey: "default" },
  setData: () => {},
  refresh: async () => {},
  error: "",
  scope: "fixture",
};
function Fixture() {
  const [opened, setOpened] = useState(false);
  const [integrated, setIntegrated] = useState(false);
  const [warningAccount, setWarningAccount] = useState("default");
  window.setRuntimeOpen = setOpened;
  window.setWarningAccount = setWarningAccount;
  return (
    <MantineProvider>
      <main style={{ maxWidth: 600, padding: 12 }}>
        <NativeRuntimeStatus
          opened={opened && !integrated}
          accounts={accounts}
        />
        <Button onClick={() => setIntegrated(true)}>Manage accounts</Button>
        <Modal
          opened={integrated}
          onClose={() => setIntegrated(false)}
          title="Accounts"
        >
          <Accounts
            managerOnly
            state={
              accountsState as React.ComponentProps<typeof Accounts>["state"]
            }
            onError={() => {}}
          />
        </Modal>
        <StudioErrorsProvider>
          <ConversationWarnings
            scope="version-warning"
            notices={notices}
            accountKey={warningAccount}
            messages={[]}
          />
        </StudioErrorsProvider>
      </main>
    </MantineProvider>
  );
}
createRoot(fixtureRoot!).render(<Fixture />);
