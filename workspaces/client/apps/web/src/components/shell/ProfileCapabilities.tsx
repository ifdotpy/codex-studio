import { NativeSelect, Stack, Text, Textarea, TextInput } from "@mantine/core";
import {
  capabilityLists,
  profileCapabilityRules,
  type CapabilityDraft,
  type CapabilityErrors,
  type CapabilityField,
} from "./profileCapabilityFields";

type Props = {
  value: CapabilityDraft;
  errors: CapabilityErrors;
  onChange: (field: CapabilityField, value: string | null) => void;
};

export function ProfileCapabilities({ value, errors, onChange }: Props) {
  return (
    <Stack gap="sm">
      <Text fw={600} size="sm">
        Worker capabilities
      </Text>
      <Text size="xs" c="dimmed">
        Inherit uses the provider default. An explicit empty list overrides that
        default. Codex does not support tool restrictions or skill preload.
      </Text>
      {capabilityLists.map(({ field, label, empty, patterns }) => (
        <Stack key={field} gap="xs">
          <NativeSelect
            label={label}
            value={value[field] === null ? "inherit" : "explicit"}
            onChange={(event) =>
              onChange(
                field,
                event.currentTarget.value === "inherit" ? null : "",
              )
            }
            data={[
              { value: "inherit", label: "Inherit (provider default)" },
              { value: "explicit", label: "Explicit list (blank means empty)" },
            ]}
          />
          {value[field] !== null && (
            <Textarea
              label={`${label} ${patterns ? "patterns" : "names"}`}
              description={`Enter one ${patterns ? "tool pattern" : "name"} per line.${patterns ? " Only * is a wildcard." : " Wildcards are not allowed."}`}
              value={value[field]}
              onChange={(event) => onChange(field, event.currentTarget.value)}
              minRows={2}
              maxRows={6}
              autosize
              error={errors[field]}
            />
          )}
          {value[field] !== null && !value[field].trim() && (
            <Text size="xs" c="dimmed">
              Explicit empty: {empty}
            </Text>
          )}
        </Stack>
      ))}
      <TextInput
        label="Maximum turns"
        description={`Completed provider turns, from ${profileCapabilityRules.minTurns} to ${profileCapabilityRules.maxTurns}. Leave blank to inherit the default.`}
        placeholder="Inherit (provider default)"
        inputMode="numeric"
        value={value.maxTurns ?? ""}
        onChange={(event) =>
          onChange("maxTurns", event.currentTarget.value || null)
        }
        error={errors.maxTurns}
      />
    </Stack>
  );
}
