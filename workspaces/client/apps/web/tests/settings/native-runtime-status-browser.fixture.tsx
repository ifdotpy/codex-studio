import { required } from "./fixture-helpers.ts";
import type { components } from "../../src/generated/api.ts";
import { emitRuntimeChange } from "./native-runtime-resource-events.fixture.ts";
import { fixtureAccount, fixtureNotReady } from "./fixture-helpers.ts";
import type { AccountsState } from "../../src/components/Accounts.tsx";
import type { ResourceRef } from "../../src/sync/resourceEvents.ts";
type NativeRuntimeHarness = {
  ready: boolean;
  required: typeof required;
  setRuntimeOpen: (opened: boolean) => void;
  setWarningAccount: (key: string) => void;
  emitRuntimeChange: (kind: ResourceRef["kind"]) => void;
  delayedRuntimeSignal: AbortSignal;
  finishOldRuntimeRead: ((response: Response) => void) | null;
};
declare global {
  interface Window {
    nativeRuntimeFixture: NativeRuntimeHarness;
  }
}
window.nativeRuntimeFixture = {
  ready: false,
  required,
  setRuntimeOpen: fixtureNotReady,
  setWarningAccount: fixtureNotReady,
  emitRuntimeChange,
  delayedRuntimeSignal: new AbortController().signal,
  finishOldRuntimeRead: null,
};
const fixtureRoot = document.getElementById("root");
if (!fixtureRoot) throw new Error("Missing fixture root");
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
const notices: components["schemas"]["NativeNoticeRecord"][] = [
  {
    id: "codex-warning",
    accountKey: "default",
    message: "Codex account version warning",
    at: 0,
    connectionId: "fixture",
  },
  {
    id: "claude-warning",
    accountKey: "claude",
    message: "Claude account version warning",
    at: 0,
    connectionId: "fixture",
  },
];
const accountsData = {
  accounts,
  defaultAccountKey: "default",
  archivedAccounts: [],
  defaultAccountGroups: [],
  logins: [],
  supportsDelete: false,
  supportsDisconnect: false,
} satisfies AccountsState;
function Fixture() {
  window.nativeRuntimeFixture.ready = true;
  const [opened, setOpened] = useState(false);
  const [integrated, setIntegrated] = useState(false);
  const [warningAccount, setWarningAccount] = useState("default");
  window.nativeRuntimeFixture.setRuntimeOpen = setOpened;
  window.nativeRuntimeFixture.setWarningAccount = setWarningAccount;
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
            state={{
              data: accountsData,
              setData: () => {},
              refresh: async () => accountsData,
              error: "",
              scope: "fixture",
            }}
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
createRoot(fixtureRoot).render(<Fixture />);
