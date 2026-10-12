import { Switch } from "@mantine/core";
import { SettingsRow } from "./ui/primitives";

export default function ProviderUpdateSettings({
  checked,
  onChange,
}: {
  checked: boolean;
  onChange: (checked: boolean) => void;
}) {
  return (
    <SettingsRow
      label="Provider release checks"
      help="Check npm for newer Claude Code and Codex CLI releases."
    >
      <Switch
        aria-label="Check for provider releases"
        checked={checked}
        onChange={(event) => onChange(event.currentTarget.checked)}
      />
    </SettingsRow>
  );
}
