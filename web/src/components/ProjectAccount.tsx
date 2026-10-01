import { Button, Checkbox, NativeSelect } from "@mantine/core";
import { useRef, useState } from "react";
import { api, errorText } from "../api";
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
  const activeAccounts = accounts.accounts;
  const activeKeys = new Set(activeAccounts.map((account) => account.id));
  const archivedMemberships = (accounts.archivedAccounts || []).filter(
    (account) => keys.includes(account.id),
  );
  const selectableKeys = keys.filter((accountKey) => {
    const account = activeAccounts.find((item) => item.id === accountKey);
    return account?.status === "ready" && !account.disconnected;
  });
  const displayedKey = selectableKeys.includes(key)
    ? key
    : selectableKeys[0] || "";
  const [pending, setPending] = useState(false);
  const [error, setError] = useState("");
  const lock = useRef(false);
  const revision = useRef(project?.accountRevision || 0);
  return (
    <form
      onSubmit={async (event) => {
        event.preventDefault();
        if (lock.current) return;
        lock.current = true;
        setPending(true);
        setError("");
        try {
          await api("/api/projects", {
            action: "set_accounts",
            account_keys: keys,
            path,
            account_key: key,
            expected_revision: revision.current,
          });
          await saved();
        } catch (error) {
          setError(errorText(error));
        } finally {
          lock.current = false;
          setPending(false);
        }
      }}
    >
      <p style={{ overflowWrap: "anywhere" }}>{path}</p>
      <Checkbox.Group
        label="Accounts shown first for this project"
        value={keys}
        onChange={(next) => {
          setKeys(next);
          if (!next.includes(key) || !activeKeys.has(key)) {
            setKey(
              next.find((accountKey) => {
                const account = activeAccounts.find(
                  (item) => item.id === accountKey,
                );
                return account?.status === "ready" && !account.disconnected;
              }) ||
                next[0] ||
                "",
            );
          }
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
                  pending ||
                  (account.status !== "ready" && !keys.includes(account.id))
                }
              />
            ))}
          {archivedMemberships.map((account) => (
            <Checkbox
              key={account.id}
              value={account.id}
              label={`${account.email || account.label || account.id} (deleted, remove from project)`}
              disabled={pending}
            />
          ))}
        </div>
      </Checkbox.Group>
      <p className="notice">Shown first in the chat account menu.</p>
      <NativeSelect
        label="Default account for new chats"
        value={displayedKey}
        disabled={pending}
        onChange={(event) => setKey(event.currentTarget.value)}
        data={activeAccounts
          .filter((account) => keys.includes(account.id))
          .map((account) => ({
            value: account.id,
            label: account.email || account.label || account.id,
            disabled: account.status !== "ready" || account.disconnected,
          }))}
      />
      <p className="notice">New chats in this project use this account.</p>
      {error && (
        <p role="alert" className="account-action-error">
          {error}
        </p>
      )}
      <Button
        type="submit"
        loading={pending}
        disabled={
          !keys.length ||
          !key ||
          (activeKeys.has(key) &&
            !!activeAccounts.find((account) => account.id === key)
              ?.disconnected)
        }
      >
        Save accounts
      </Button>
    </form>
  );
}
