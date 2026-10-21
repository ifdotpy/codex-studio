import { useServerApi } from "../servers/ServerServicesContext";
import { Button, Group, TextInput } from "@mantine/core";
import { useRef, useState } from "react";
import { errorText, type PostResult } from "../api";
import { parseGroups } from "./accountPreference";
import type { Account } from "./Accounts";

/** The group of one account on one Studio server. Project account orders list
 * group names; `serverLabel` names the server where an account is on several. */
export default function AccountGroup({
  account,
  serverLabel,
  onSaved,
}: {
  account: Account;
  serverLabel?: string;
  onSaved: (state: PostResult<"/api/accounts/group">) => void;
}) {
  const scope = serverLabel ? ` on ${serverLabel}` : "";
  const { post } = useServerApi();
  const [text, setText] = useState(account.group || "");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [saved, setSaved] = useState(false);
  const lock = useRef(false);
  const parsed = parseGroups(text);
  const invalid =
    parsed.error ||
    (parsed.groups.length > 1 ? "Use one group name for an account." : "");
  const next = parsed.groups[0] || null;
  const changed = next !== (account.group || null);
  const save = async () => {
    if (lock.current || invalid || !changed) return;
    lock.current = true;
    setBusy(true);
    setError("");
    setSaved(false);
    try {
      onSaved(
        await post("/api/accounts/group", {
          account_key: account.id,
          group: next,
        }),
      );
      setSaved(true);
    } catch (failure) {
      setError(errorText(failure));
    } finally {
      lock.current = false;
      setBusy(false);
    }
  };
  return (
    <form
      className="account-group"
      onSubmit={(event) => {
        event.preventDefault();
        void save();
      }}
    >
      <Group gap="xs" align="flex-end" wrap="nowrap">
        <TextInput
          label={`Group${scope}`}
          aria-label={`Group of ${account.email || account.label}${scope}`}
          description="Project account orders use this name, for example personal."
          placeholder="No group"
          value={text}
          maxLength={32}
          disabled={busy}
          error={invalid || undefined}
          onChange={(event) => {
            setSaved(false);
            setText(event.currentTarget.value);
          }}
        />
        {changed && (
          <Button
            type="submit"
            variant="light"
            loading={busy}
            disabled={!!invalid}
          >
            Save group
          </Button>
        )}
      </Group>
      {error && (
        <p className="account-action-error" role="alert">
          {error}
        </p>
      )}
      {saved && <p role="status">Group saved.</p>}
    </form>
  );
}
