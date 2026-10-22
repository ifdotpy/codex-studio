import { Stack, Switch } from "@mantine/core";

type Props = {
  disabled: boolean;
  fastAvailable: boolean;
  fast: boolean;
  onFast: () => void;
  daybreakAvailable: boolean;
  daybreak: boolean;
  onDaybreak: () => void;
};

export function ModeToggles(p: Props) {
  return p.fastAvailable || p.daybreakAvailable ? (
    <Stack gap="xs" aria-label="Optional modes">
      {p.fastAvailable && (
        <Switch
          size="sm"
          label="Fast mode"
          aria-label="Fast mode"
          description="Faster responses with increased usage."
          checked={p.fast}
          disabled={p.disabled}
          onChange={p.onFast}
        />
      )}
      {p.daybreakAvailable && (
        <Switch
          size="sm"
          label="Daybreak"
          aria-label="Daybreak"
          description="Use the selected model with Daybreak access."
          checked={p.daybreak}
          disabled={p.disabled}
          onChange={p.onDaybreak}
        />
      )}
    </Stack>
  ) : null;
}
