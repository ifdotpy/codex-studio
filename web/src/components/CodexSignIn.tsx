import { Modal } from "@mantine/core";
import type { Account, useAccounts } from "./Accounts";
import AccountSignIn from "./AccountSignIn";

export default function CodexSignIn({ account, state, onClose }: {
  account: Account;
  state: ReturnType<typeof useAccounts>;
  onClose: () => void;
}) {
  return <Modal opened onClose={onClose} title={`Sign in to ${account.email || account.label}`}>
    <AccountSignIn key={`${state.scope}:${account.id}`} state={state} opened targetAccount={account} />
  </Modal>;
}
