import { Modal } from "@mantine/core";
import type { Account, useAccounts } from "./Accounts";
import AccountSignIn from "./AccountSignIn";
import { post } from "../api";

export default function CodexSignIn({
  account,
  state,
  onClose,
  serverLabel,
}: {
  account: Account;
  state: ReturnType<typeof useAccounts>;
  onClose: () => void;
  serverLabel?: string;
}) {
  return (
    <Modal
      opened
      onClose={onClose}
      title={`Sign in to Codex · ${account.email || account.label}${serverLabel ? ` · ${serverLabel}` : ""}`}
    >
      <AccountSignIn
        key={`${state.scope}:${account.id}`}
        state={state}
        targetAccount={account}
        onConnected={async (connected) => {
          if (connected.disconnected)
            state.setData(
              await post("/api/accounts/reconnect", {
                account_key: connected.id,
              }),
            );
        }}
      />
    </Modal>
  );
}
