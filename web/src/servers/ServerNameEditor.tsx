import { useEffect, useState } from "react";
import { Group, TextInput } from "@mantine/core";
import { ActionButton } from "../components/ui/primitives";
import { validateServerName } from "./serverNames";

export default function ServerNameEditor({
  value,
  disabled,
  save,
}: {
  value: string;
  disabled?: boolean;
  save: (label: string) => Promise<boolean>;
}) {
  const [label, setLabel] = useState(value);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  useEffect(() => {
    setLabel(value);
  }, [value]);
  return (
    <form
      onSubmit={(event) => {
        event.preventDefault();
        setError("");
        try {
          validateServerName(label);
        } catch (failure) {
          setError((failure as Error).message);
          return;
        }
        setBusy(true);
        void save(label)
          .then((ok) => {
            if (!ok)
              setError("Check the server name or retry the saved request.");
          })
          .catch((failure) => setError((failure as Error).message))
          .finally(() => setBusy(false));
      }}
    >
      <Group align="flex-end" gap="xs">
        <TextInput
          label="Server name"
          description="Use 1 to 80 visible characters."
          value={label}
          onChange={(event) => {
            setLabel(event.currentTarget.value);
            setError("");
          }}
          error={error}
          disabled={disabled || busy}
          autoComplete="off"
          style={{ flex: 1, minWidth: 160 }}
        />
        <ActionButton
          type="submit"
          actionRole="secondary"
          loading={busy}
          disabled={disabled || label === value}
        >
          Save name
        </ActionButton>
      </Group>
    </form>
  );
}
