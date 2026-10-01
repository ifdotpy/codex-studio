import { Button, Checkbox, NativeSelect } from "@mantine/core";
import { useRef, useState } from "react";
import { useProjectSave } from "./useProjectSave";
import type { Snapshot } from "../types";
import type { AccountsState } from "./Accounts";

export default function ProjectAccount({
  path,
  project,
  defaultAccountKey,
  accounts,
  saved,
}: {
  path: string;
  project?: NonNullable<Snapshot["runtime"]["projects"]>[number];
  accounts: AccountsState;
  defaultAccountKey: string;
  saved: () => Promise<void>;
}) {
  const [key, setKey] = useState(project?.accountKey || defaultAccountKey);
  const [keys, setKeys] = useState(
    project?.accountKeys || [project?.accountKey || defaultAccountKey],
  );
  const selectableKeys = keys.filter((accountKey) => {
    const account = accounts.accounts.find((item) => item.id === accountKey);
    return account?.status === "ready" && !account.disconnected;
  });
  const displayedKey = selectableKeys.includes(key) ? key : "";
  const save = useProjectSave("/api/projects", saved);
  const activeAccounts = accounts.accounts;
  const archivedMemberships = (accounts.archivedAccounts || []).filter(
    (account) => keys.includes(account.id),
  );
  const selected = accounts.accounts.find((account) => account.id === key);
  const ready = selected?.status === "ready" && !selected.disconnected;
  const revision = useRef(project?.accountRevision || 0);
  return (
    <form
      onSubmit={(event) => {
        event.preventDefault();
        if (!save.frozen && !ready) return;
        void save.submit({
          action: "set_accounts",
          account_keys: keys,
          path,
          account_key: key,
          expected_revision: revision.current,
        });
      }}
    >
      <p style={{ overflowWrap: "anywhere" }}>{path}</p>
      <Checkbox.Group
        label="Accounts shown first for this project"
        value={keys}
        onChange={(next) => {
          setKeys(next);
          if (!next.includes(key)) setKey("");
        }}
      >
        <div style={{ display: "grid", gap: 12, margin: "12px 0 20px" }}>
          {activeAccounts
            .filter(
              (account) => !account.disconnected || keys.includes(account.id),
            )
            .map((account) => (
              <Checkbox
                key={account.id}
                value={account.id}
                label={`${account.email || account.label || account.id}${account.disconnected ? " (disconnected)" : ""}`}
                disabled={
                  save.pending ||
                  save.frozen ||
                  (account.status !== "ready" && !keys.includes(account.id))
                }
              />
            ))}
          {archivedMemberships.map((account) => (
            <Checkbox
              key={account.id}
              value={account.id}
              label={`${account.email || account.label || account.id} (deleted, remove from project)`}
              disabled={save.pending || save.frozen}
            />
          ))}
        </div>
      </Checkbox.Group>
      <p className="notice">Shown first in the chat account menu.</p>
      <NativeSelect
        label="Default account for new chats"
        value={displayedKey}
        disabled={save.pending || save.frozen}
        onChange={(event) => setKey(event.currentTarget.value)}
        data={[
          {
            value: "",
            label: "Select a connected project account",
            disabled: true,
          },
          ...activeAccounts
            .filter((account) => keys.includes(account.id))
            .map((account) => ({
              value: account.id,
              label: account.email || account.label || account.id,
              disabled: account.status !== "ready" || account.disconnected,
            })),
        ]}
      />
      <p className="notice">New chats in this project use this account.</p>
      {!ready && !save.frozen && (
        <p role="status">Choose an account that is ready before you save.</p>
      )}
      {save.error && (
        <p role="alert" className="account-action-error">
          {save.error}
        </p>
      )}
      <Button
        type="submit"
        loading={save.pending}
        disabled={!save.frozen && (!keys.length || !key || !ready)}
      >
        {save.retryLabel || "Save accounts"}
      </Button>
    </form>
  );
}
