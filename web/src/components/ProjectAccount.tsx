import { Button, TextInput } from "@mantine/core";
import { AccountTiles } from "./AccountTiles";
import { useRef, useState } from "react";
import { useProjectSave } from "./useProjectSave";
import type { Snapshot } from "../types";
import type { AccountsState } from "./Accounts";

type SnapshotProject = NonNullable<
  NonNullable<Snapshot["runtime"]>["projects"]
>[number];

export default function ProjectAccount({
  path,
  project,
  defaultAccountKey,
  accounts,
  saved,
}: {
  path: string;
  project?: SnapshotProject;
  accounts: AccountsState;
  defaultAccountKey: string;
  saved: () => Promise<void>;
}) {
  const [key, setKey] = useState(project?.accountKey || defaultAccountKey);
  const [keys, setKeys] = useState(
    project?.accountKeys || [project?.accountKey || defaultAccountKey],
  );
  const [workerBase, setWorkerBase] = useState(project?.workerBaseRef || "");
  const selectableKeys = keys.filter((accountKey) => {
    const account = accounts.accounts.find((item) => item.id === accountKey);
    return account?.status === "ready" && !account.disconnected;
  });
  const displayedKey = selectableKeys.includes(key) ? key : "";
  const save = useProjectSave("/api/projects", saved);
  const saveWorkerBase = useProjectSave("/api/projects", saved);
  const activeAccounts = accounts.accounts;
  const archivedMemberships = (accounts.archivedAccounts || []).filter(
    (account) => keys.includes(account.id),
  );
  const selected = accounts.accounts.find((account) => account.id === key);
  const ready = selected?.status === "ready" && !selected.disconnected;
  const revision = useRef(project?.accountRevision || 0);
  const workerBaseRevision = useRef(project?.workerBaseRevision || 0);
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
      <AccountTiles
        label="Accounts shown first for this project"
        multiple
        providers={[
          ...new Set(
            [
              ...activeAccounts.filter(
                (account) => !account.disconnected || keys.includes(account.id),
              ),
              ...archivedMemberships,
            ].map((account) => account.provider || "codex"),
          ),
        ]}
        accounts={[
          ...activeAccounts.filter(
            (account) => !account.disconnected || keys.includes(account.id),
          ),
          ...archivedMemberships,
        ]}
        value={keys}
        disabled={save.pending || save.frozen}
        accountDisabled={(account) =>
          account.status !== "ready" && !keys.includes(account.id)
        }
        onChange={(next) => {
          setKeys(next);
          if (!next.includes(key)) setKey("");
        }}
      />
      <p className="notice">Shown first in the chat account menu.</p>
      <AccountTiles
        label="Default account for new chats"
        accounts={activeAccounts.filter((account) => keys.includes(account.id))}
        value={displayedKey}
        disabled={save.pending || save.frozen}
        accountDisabled={(account) =>
          account.status !== "ready" || !!account.disconnected
        }
        onChange={setKey}
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
      <TextInput
        mt="lg"
        label="Default worker base ref"
        description="Use a branch, tag, or commit for new worker worktrees. Leave empty to use repository HEAD."
        placeholder="main or origin/main"
        value={workerBase}
        maxLength={1024}
        disabled={saveWorkerBase.pending || saveWorkerBase.frozen}
        onChange={(event) => setWorkerBase(event.currentTarget.value)}
      />
      {saveWorkerBase.error && (
        <p role="alert" className="account-action-error">
          {saveWorkerBase.error}
        </p>
      )}
      <Button
        mt="md"
        type="button"
        loading={saveWorkerBase.pending}
        disabled={saveWorkerBase.frozen}
        onClick={() =>
          void saveWorkerBase.submit({
            action: "set_worker_base",
            path,
            base_ref: workerBase.trim() || null,
            expected_revision: workerBaseRevision.current,
          })
        }
      >
        {saveWorkerBase.retryLabel || "Save worker base"}
      </Button>
    </form>
  );
}
