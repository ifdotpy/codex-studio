import { NativeSelect, TextInput } from "@mantine/core";
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
  const [environment, setEnvironment] = useState(
    project?.workerEnvironment || "host",
  );
  const selectableKeys = keys.filter((accountKey) => {
    const account = accounts.accounts.find((item) => item.id === accountKey);
    return account?.status === "ready" && !account.disconnected;
  });
  const displayedKey = selectableKeys.includes(key) ? key : "";
  const save = useProjectSave("/api/projects", saved);
  const saveWorkerBase = useProjectSave("/api/projects", saved);
  const saveEnvironment = useProjectSave("/api/projects", saved);
  const activeAccounts = accounts.accounts;
  const archivedMemberships = (accounts.archivedAccounts || []).filter(
    (account) => keys.includes(account.id),
  );
  const selected = accounts.accounts.find((account) => account.id === key);
  const ready = selected?.status === "ready" && !selected.disconnected;
  const revision = useRef(project?.accountRevision || 0);
  const workerBaseRevision = useRef(project?.workerBaseRevision || 0);
  const environmentRevision = useRef(project?.workerEnvironmentRevision || 0);
  const accountsChanged =
    key !== (project?.accountKey || defaultAccountKey) ||
    JSON.stringify(keys) !==
      JSON.stringify(
        project?.accountKeys || [project?.accountKey || defaultAccountKey],
      );
  const workerBaseChanged =
    workerBase.trim() !== (project?.workerBaseRef || "");
  const environmentChanged =
    environment !== (project?.workerEnvironment || "host");
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
        <SettingsRow label="Default account">
          <AccountTiles
            label="Default account for new chats"
            showLabel={false}
            accounts={activeAccounts.filter((account) =>
              keys.includes(account.id),
            )}
            value={displayedKey}
            disabled={save.pending || save.frozen}
            accountDisabled={(account) =>
              account.status !== "ready" || !!account.disconnected
            }
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
              account.status !== "ready" && !keys.includes(account.id)
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
      <form
        className="project-settings-group"
        onSubmit={(event) => {
          event.preventDefault();
          if (
            saveEnvironment.pending ||
            (saveEnvironment.frozen && !saveEnvironment.retryLabel)
          )
            return;
          if (!saveEnvironment.frozen && !environmentChanged) return;
          void saveEnvironment.submit({
            action: "set_worker_environment",
            path,
            environment,
            expected_revision: environmentRevision.current,
          });
        }}
      >
        <SettingsRow
          label={
            <label htmlFor="project-worker-environment">
              Worker environment
            </label>
          }
        >
          <NativeSelect
            id="project-worker-environment"
            aria-label="Default worker environment"
            aria-description="New workers use this environment. Leads and reviewers use the host."
            value={environment}
            disabled={saveEnvironment.pending || saveEnvironment.frozen}
            onChange={(event) =>
              setEnvironment(event.currentTarget.value as "host" | "linux")
            }
            data={[
              { value: "host", label: "Host" },
              { value: "linux", label: "Linux VM" },
            ]}
          />
        </SettingsRow>
        {saveEnvironment.error && <p role="alert">{saveEnvironment.error}</p>}
        {(saveEnvironment.frozen || environmentChanged) && (
          <ActionButton
            aria-label="Save worker environment"
            actionRole={
              accountsChanged ||
              save.frozen ||
              workerBaseChanged ||
              saveWorkerBase.frozen
                ? "secondary"
                : "primary"
            }
            type="submit"
            loading={saveEnvironment.pending}
            disabled={saveEnvironment.frozen && !saveEnvironment.retryLabel}
          >
            {saveEnvironment.retryLabel || "Save"}
          </ActionButton>
        )}
      </form>
    </div>
  );
}
