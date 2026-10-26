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
const account: Partial<Account> = {
  id: "claude",
  label: "Personal",
  email: "me@example.com",
  provider: "claude",
  status: "changed",
  error:
    "This profile's account changed. Restore its original login or add a separate profile.",
} satisfies Pick<
  Account,
  "id" | "label" | "email" | "provider" | "status" | "error"
>;
const state: Omit<
  React.ComponentProps<typeof Accounts>["state"],
  "data" | "refresh"
> & {
  data: Omit<Partial<AccountsState>, "accounts"> & {
    accounts: Partial<Account>[];
  };
  refresh: () => Promise<AccountsState | null | void>;
} = {
  scope: "fixture",
  data: {
    accounts: [account],
    defaultAccountKey: "claude",
  },
  setData: () => {},
  refresh: async (): Promise<AccountsState | null | void> => {},
  error: "",
};
// The base partial fixture omits account home/source and Accounts collection/capability fields.
const partialAccountsFixture = { account, state } as {
  account: Account;
  state: React.ComponentProps<typeof Accounts>["state"];
};
function Fixture() {
  const [opened, setOpened] = useState(false);
  return (
    <MantineProvider>
      <AccountSignInNotice
        account={partialAccountsFixture.account}
        errors={[{ message: "An unrelated earlier error" }]}
        onSignIn={(key) => (window.signInAccount = key)}
      />
      <Button onClick={() => setOpened(true)}>Manage accounts</Button>
      <Modal opened={opened} onClose={() => setOpened(false)} title="Accounts">
        <Accounts
          managerOnly
          state={partialAccountsFixture.state}
          onError={() => {}}
        />
      </Modal>
    </MantineProvider>
  );
}
createRoot(document.getElementById("root")!).render(<Fixture />);
