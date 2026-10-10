import { useEffect, useState } from "react";
import { Group, TextInput } from "@mantine/core";
import { ActionButton } from "../components/ui/primitives";
import { validateServerAlias } from "./serverAliases";

export default function ServerAliasEditor({
  value,
  used,
  disabled,
  save,
}: {
  value: string;
  used: string[];
  disabled?: boolean;
  save: (alias: string) => Promise<boolean>;
}) {
  const [alias, setAlias] = useState(value);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  useEffect(() => {
    setAlias(value);
  }, [value]);
  return (
    <form
      onSubmit={(event) => {
        event.preventDefault();
        setError("");
        try {
          validateServerAlias(alias, used);
        } catch (failure) {
          setError((failure as Error).message);
          return;
        }
        setBusy(true);
        void save(alias)
          .then((ok) => {
            if (!ok)
              setError(
                "The alias result is unknown. Check server settings or retry the saved request.",
              );
          })
          .catch((failure) => setError((failure as Error).message))
          .finally(() => setBusy(false));
      }}
    >
      <Group align="flex-end" gap="xs">
        <TextInput
          label="Server alias"
          description="Use 1 to 3 uppercase letters."
          value={alias}
          onChange={(event) => {
            setAlias(event.currentTarget.value);
            setError("");
          }}
          error={error}
          disabled={disabled || busy}
          autoComplete="off"
          style={{ width: 190 }}
        />
        <ActionButton
          type="submit"
          actionRole="secondary"
          loading={busy}
          disabled={disabled || alias === value}
        >
          Save alias
        </ActionButton>
      </Group>
    </form>
  );
}
