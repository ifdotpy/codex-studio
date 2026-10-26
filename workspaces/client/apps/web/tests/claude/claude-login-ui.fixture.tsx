import type { Account, AccountsState } from "../../src/components/Accounts.tsx";
declare global {
  interface Window {
    signInAccount: string;
  }
}
import React, { useState } from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider, Modal, Button } from "@mantine/core";
import "@mantine/core/styles.css";
import Accounts from "../../src/components/Accounts.tsx";
import AccountSignInNotice from "../../src/components/AccountSignInNotice.tsx";
const account: Account = {
  home: "/fixture",
  source: "fixture",
  id: "claude",
  label: "Personal",
  email: "me@example.com",
  provider: "claude",
  status: "changed",
  error:
    "This profile's account changed. Restore its original login or add a separate profile.",
};
const state = {
  scope: "fixture",
  data: {
    accounts: [account],
    defaultAccountKey: "claude",
    archivedAccounts: [],
    defaultAccountGroups: [],
    logins: [],
    supportsDelete: false,
    supportsDisconnect: false,
  } satisfies AccountsState,
  setData: () => {},
  refresh: async () => null,
  error: "",
};
function Fixture() {
  const [opened, setOpened] = useState(false);
  return (
    <MantineProvider>
      <AccountSignInNotice
        account={account}
        errors={[{ message: "An unrelated earlier error" }]}
        onSignIn={(key) => (window.signInAccount = key)}
      />
      <Button onClick={() => setOpened(true)}>Manage accounts</Button>
      <Modal opened={opened} onClose={() => setOpened(false)} title="Accounts">
        <Accounts managerOnly state={state} onError={() => {}} />
      </Modal>
    </MantineProvider>
  );
}
const root = document.getElementById("root");
if (!root) throw new Error("Missing fixture root");
createRoot(root).render(<Fixture />);
