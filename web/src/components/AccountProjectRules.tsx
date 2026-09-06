import { Button, Radio, Textarea } from "@mantine/core";
import { FolderLock } from "lucide-react";
import { useState } from "react";
import { api, errorText } from "../api";
import type { Account, AccountsState } from "./Accounts";

export function projectRuleSummary(account: Account) {
  const paths = account.projectRules?.allowedProjects;
  if (paths == null) return "All projects";
  if (!paths.length) return "No projects allowed";
  return paths
    .map((path) => path.split("/").filter(Boolean).at(-1) || "/")
    .join(", ");
}

export default function AccountProjectRules({
  account,
  saved,
}: {
  account: Account;
  saved: (state: AccountsState) => void;
}) {
  const [editing, setEditing] = useState(false);
  const [mode, setMode] = useState("all");
  const [paths, setPaths] = useState("");
  const [revision, setRevision] = useState(0);
  const [pending, setPending] = useState(false);
  const [error, setError] = useState("");
  const begin = () => {
    const rules = account.projectRules;
    setMode(rules?.allowedProjects == null ? "all" : "only");
    setPaths((rules?.allowedProjects || []).join("\n"));
    setRevision(rules?.revision || 0);
    setError("");
    setEditing(true);
  };
  return (
    <div className="account-project-rules">
      <div className="account-rules-summary">
        <FolderLock size={13} />
        <span>
          {account.projectRules?.allowedProjects?.length
            ? "Allowed projects"
            : projectRuleSummary(account)}
        </span>
        <Button
          size="compact-xs"
          variant="subtle"
          onClick={begin}
          disabled={editing}
          aria-label={`Edit rules for ${account.email || account.label}`}
        >
          Edit rules
        </Button>
      </div>
      {!!account.projectRules?.allowedProjects?.length && (
        <div className="account-rule-paths">
          {account.projectRules.allowedProjects.map((path) => (
            <code key={path}>{path}</code>
          ))}
        </div>
      )}
      {editing && (
        <form
          className="account-rule-editor"
          onSubmit={async (event) => {
            event.preventDefault();
            if (pending) return;
            setPending(true);
            setError("");
            try {
              saved(
                await api<AccountsState>("/api/accounts/rules", {
                  account_key: account.id,
                  allowed_projects:
                    mode === "all"
                      ? null
                      : paths
                          .split("\n")
                          .map((path) => path.trim())
                          .filter(Boolean),
                  expected_revision: revision,
                }),
              );
              setEditing(false);
            } catch (error) {
              setError(errorText(error));
            } finally {
              setPending(false);
            }
          }}
        >
          <Radio.Group
            label="Projects this account can use"
            value={mode}
            onChange={setMode}
          >
            <Radio value="all" label="All projects" disabled={pending} />
            <Radio
              value="only"
              label="Only these projects"
              disabled={pending}
            />
          </Radio.Group>
          {mode === "only" && (
            <Textarea
              label="Allowed project folders"
              description="One absolute folder path per line. Subfolders are included. An empty list blocks every project."
              placeholder="/path/to/project"
              value={paths}
              onChange={(event) => setPaths(event.target.value)}
              disabled={pending}
              autosize
              minRows={2}
              maxRows={6}
            />
          )}
          {error && (
            <p role="alert" className="account-action-error">
              {error} Your draft is kept. Cancel and reopen to load the latest
              rules.
            </p>
          )}
          <div className="account-rule-actions">
            <Button size="compact-xs" type="submit" loading={pending}>
              Save rules
            </Button>
            <Button
              size="compact-xs"
              variant="subtle"
              disabled={pending}
              onClick={() => setEditing(false)}
            >
              Cancel
            </Button>
          </div>
        </form>
      )}
    </div>
  );
}
