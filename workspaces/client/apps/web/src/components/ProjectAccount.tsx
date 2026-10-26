import { accountCanStart, accountDisplayName } from "../accountName";
import { newChatDefault, parseGroups } from "./accountPreference";
import { TextInput } from "@mantine/core";
import { AccountTiles } from "./AccountTiles";
import { ActionButton, SettingsRow } from "./ui/primitives";
import "./project-settings.css";
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
  project?: SnapshotProject | undefined;
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
    return accountCanStart(account);
  });
  const displayedKey = selectableKeys.includes(key) ? key : "";
  const save = useProjectSave("/api/projects", saved);
  const saveWorkerBase = useProjectSave("/api/projects", saved);
  const activeAccounts = accounts.accounts;
  const archivedMemberships = (accounts.archivedAccounts || []).filter(
    (account) => keys.includes(account.id),
  );
  const selected = accounts.accounts.find((account) => account.id === key);
  const ready = accountCanStart(selected);
  const revision = useRef(project?.accountRevision || 0);
  const workerBaseRevision = useRef(project?.workerBaseRevision || 0);
  const accountsChanged =
    key !== (project?.accountKey || defaultAccountKey) ||
    JSON.stringify(keys) !==
      JSON.stringify(
        project?.accountKeys || [project?.accountKey || defaultAccountKey],
      );
  const workerBaseChanged =
    workerBase.trim() !== (project?.workerBaseRef || "");
  const savedOrder = (project?.accountGroups || []).join(", ");
  const savedDefaultOrder = (accounts.defaultAccountGroups || []).join(", ");
  const [order, setOrder] = useState(savedOrder);
  const [defaultOrder, setDefaultOrder] = useState(savedDefaultOrder);
  const saveOrder = useProjectSave("/api/projects", saved);
  const saveDefaultOrder = useProjectSave(
    "/api/accounts/default-groups",
    saved,
  );
  const orderRevision = useRef(project?.accountGroupsRevision || 0);
  const parsedOrder = parseGroups(order);
  const parsedDefaultOrder = parseGroups(defaultOrder);
  const orderChanged =
    parsedOrder.groups.join(",") !== (project?.accountGroups || []).join(",");
  const defaultOrderChanged =
    parsedDefaultOrder.groups.join(",") !==
    (accounts.defaultAccountGroups || []).join(",");
  const knownGroups = [
    ...new Set(
      accounts.accounts
        .map((account) => account.group)
        .filter((group): group is string => !!group),
    ),
  ].sort();
  // The account that a new chat of this project starts on with these orders.
  const preview = newChatDefault(
    accounts.accounts,
    accounts.defaultAccountKey,
    parsedDefaultOrder.groups,
    {
      accountKey: project?.accountKey,
      accountKeys: project?.accountKeys,
      accountGroups: parsedOrder.groups,
    },
    null,
  );
  const previewAccount = accounts.accounts.find(
    (account) => account.id === preview.accountKey,
  );
  return (
    <div className="project-settings">
      <p className="project-settings-path" title={path}>
        {path}
      </p>
      <form
        className="project-settings-group"
        onSubmit={(event) => {
          event.preventDefault();
          if (!save.frozen && (!ready || !accountsChanged)) return;
          void save.submit({
            action: "set_accounts",
            account_keys: keys,
            path,
            account_key: key,
            expected_revision: revision.current,
          });
        }}
      >
        <SettingsRow
          label="Default account"
          help="Comes first in its group of the account order. A Claude account starts new chats when one is ready."
        >
          <AccountTiles
            label="Default account for new chats"
            showLabel={false}
            accounts={activeAccounts.filter((account) =>
              keys.includes(account.id),
            )}
            value={displayedKey}
            disabled={save.pending || save.frozen}
            accountDisabled={(account) => !accountCanStart(account)}
            onChange={setKey}
          />
        </SettingsRow>
        <SettingsRow label="Shown first">
          <AccountTiles
            label="Accounts shown first for this project"
            showLabel={false}
            multiple
            providers={[
              ...new Set(
                [
                  ...activeAccounts.filter(
                    (account) =>
                      !account.disconnected || keys.includes(account.id),
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
              !accountCanStart(account) && !keys.includes(account.id)
            }
            onChange={(next) => {
              setKeys(next);
              if (!next.includes(key)) setKey("");
            }}
          />
        </SettingsRow>
        {!ready && !save.frozen && (
          <p role="status">Choose an account that is ready before you save.</p>
        )}
        {save.error && (
          <p role="alert" className="account-action-error">
            {save.error}
          </p>
        )}
        {(save.frozen || accountsChanged) && (
          <ActionButton
            aria-label="Save accounts"
            actionRole="primary"
            type="submit"
            loading={save.pending}
            disabled={!save.frozen && (!keys.length || !key || !ready)}
          >
            {save.retryLabel || "Save"}
          </ActionButton>
        )}
      </form>
      <form
        className="project-settings-group"
        onSubmit={(event) => {
          event.preventDefault();
          if (saveOrder.pending || (saveOrder.frozen && !saveOrder.retryLabel))
            return;
          if (!saveOrder.frozen && (!orderChanged || parsedOrder.error)) return;
          void saveOrder.submit({
            action: "set_account_groups",
            path,
            account_groups: parsedOrder.groups,
            expected_revision: orderRevision.current,
          });
        }}
      >
        <SettingsRow
          label={<label htmlFor="project-account-order">Account order</label>}
          help="New chats and workers prefer accounts of the first group, then the next group. Other accounts stay available."
        >
          <TextInput
            id="project-account-order"
            aria-label="Account order of this project"
            placeholder={
              savedDefaultOrder
                ? `Default order: ${savedDefaultOrder}`
                : "personal, work"
            }
            description={
              knownGroups.length
                ? `Groups on this server: ${knownGroups.join(", ")}. Leave empty to use the default order.`
                : "No account has a group yet. Set a group on each account in Settings > Accounts."
            }
            value={order}
            disabled={saveOrder.pending || saveOrder.frozen}
            error={parsedOrder.error || undefined}
            onChange={(event) => setOrder(event.currentTarget.value)}
          />
        </SettingsRow>
        {saveOrder.error && (
          <p role="alert" className="account-action-error">
            {saveOrder.error}
          </p>
        )}
        {(saveOrder.frozen || orderChanged) && (
          <ActionButton
            type="submit"
            aria-label="Save account order"
            actionRole="primary"
            loading={saveOrder.pending}
            disabled={
              saveOrder.frozen ? !saveOrder.retryLabel : !!parsedOrder.error
            }
          >
            {saveOrder.retryLabel || "Save"}
          </ActionButton>
        )}
      </form>
      <form
        className="project-settings-group"
        onSubmit={(event) => {
          event.preventDefault();
          if (
            saveDefaultOrder.pending ||
            (saveDefaultOrder.frozen && !saveDefaultOrder.retryLabel)
          )
            return;
          if (
            !saveDefaultOrder.frozen &&
            (!defaultOrderChanged || parsedDefaultOrder.error)
          )
            return;
          void saveDefaultOrder.submit({ groups: parsedDefaultOrder.groups });
        }}
      >
        <SettingsRow
          label={
            <label htmlFor="default-account-order">Default account order</label>
          }
          help="Applies to every project on this server without its own account order."
        >
          <TextInput
            id="default-account-order"
            aria-label="Default account order of this server"
            placeholder="personal, work"
            value={defaultOrder}
            disabled={saveDefaultOrder.pending || saveDefaultOrder.frozen}
            error={parsedDefaultOrder.error || undefined}
            onChange={(event) => setDefaultOrder(event.currentTarget.value)}
          />
        </SettingsRow>
        {saveDefaultOrder.error && (
          <p role="alert" className="account-action-error">
            {saveDefaultOrder.error}
          </p>
        )}
        {(saveDefaultOrder.frozen || defaultOrderChanged) && (
          <ActionButton
            type="submit"
            aria-label="Save default account order"
            actionRole="secondary"
            loading={saveDefaultOrder.pending}
            disabled={
              saveDefaultOrder.frozen
                ? !saveDefaultOrder.retryLabel
                : !!parsedDefaultOrder.error
            }
          >
            {saveDefaultOrder.retryLabel || "Save"}
          </ActionButton>
        )}
      </form>
      <p className="project-settings-path" role="status">
        {previewAccount
          ? `New chats start on ${accountDisplayName(previewAccount)} (${
              previewAccount.provider === "claude" ? "Claude" : "Codex"
            })${preview.model ? " with Opus 5.5 · Medium" : ""}.`
          : "No account can start a new chat. Sign in to an account first."}
      </p>
      <form
        className="project-settings-group"
        onSubmit={(event) => {
          event.preventDefault();
          if (
            saveWorkerBase.pending ||
            (saveWorkerBase.frozen && !saveWorkerBase.retryLabel)
          )
            return;
          if (!saveWorkerBase.frozen && !workerBaseChanged) return;
          void saveWorkerBase.submit({
            action: "set_worker_base",
            path,
            base_ref: workerBase.trim() || null,
            expected_revision: workerBaseRevision.current,
          });
        }}
      >
        <SettingsRow
          label={<label htmlFor="project-worker-base">Worker base</label>}
        >
          <TextInput
            id="project-worker-base"
            aria-label="Default worker base ref"
            aria-description="Use a branch, tag, or commit for new worker worktrees. Leave empty to use repository HEAD."
            placeholder="main"
            value={workerBase}
            maxLength={1024}
            disabled={saveWorkerBase.pending || saveWorkerBase.frozen}
            onChange={(event) => setWorkerBase(event.currentTarget.value)}
          />
        </SettingsRow>
        {saveWorkerBase.error && (
          <p role="alert" className="account-action-error">
            {saveWorkerBase.error}
          </p>
        )}
        {(saveWorkerBase.frozen || workerBaseChanged) && (
          <ActionButton
            type="submit"
            aria-label="Save worker base"
            actionRole={
              accountsChanged || save.frozen ? "secondary" : "primary"
            }
            loading={saveWorkerBase.pending}
            disabled={saveWorkerBase.frozen && !saveWorkerBase.retryLabel}
          >
            {saveWorkerBase.retryLabel || "Save"}
          </ActionButton>
        )}
      </form>
    </div>
  );
}
