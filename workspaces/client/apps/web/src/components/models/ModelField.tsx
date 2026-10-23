import { NativeSelect, Radio, Stack } from "@mantine/core";
import {
  INHERITED_MODEL,
  modelOptionLabel,
  modelForValue,
  type ModelOption,
} from "./modelOption";
import "./model-field.css";

type Props = {
  label: string;
  options: ModelOption[];
  value: string;
  onChange: (value: string) => void;
  presentation?: "list" | "select";
  id?: string;
  disabled?: boolean;
};

export function ModelField({
  label,
  options,
  value,
  onChange,
  presentation = "list",
  id,
  disabled,
}: Props) {
  const selected = modelForValue(options, value);
  const selectedValue = selected?.value ?? value;
  if (presentation === "select") {
    return (
      <NativeSelect
        id={id}
        label={label}
        size="sm"
        value={selectedValue}
        data-value={selectedValue}
        disabled={disabled}
        description={selected?.disabledReason || selected?.description}
        onChange={(event) => onChange(event.currentTarget.value)}
        className="model-select"
        data={[...new Set(options.map((option) => option.provider))].map(
          (provider) => ({
            group:
              provider === "claude"
                ? "Claude"
                : provider === "codex"
                  ? "Codex"
                  : provider,
            items: options
              .filter((option) => option.provider === provider)
              .map((option) => ({
                value: option.value,
                label: modelOptionLabel(option),
                disabled: !!option.disabledReason,
              })),
          }),
        )}
      />
    );
  }
  return (
    <Radio.Group
      id={id}
      label={label}
      value={selectedValue}
      onChange={onChange}
      size="sm"
      data-value={selectedValue}
      disabled={disabled || undefined}
    >
      <Stack gap={0} className="model-options">
        {options.map((option) => (
          <Radio
            key={option.value}
            value={option.value}
            data-value={option.value}
            data-unavailable={!!option.disabledReason || undefined}
            data-inherited={option.value === INHERITED_MODEL || undefined}
            label={modelOptionLabel(option)}
            description={option.disabledReason || option.description}
            disabled={disabled || !!option.disabledReason}
            className="model-radio"
          />
        ))}
      </Stack>
    </Radio.Group>
  );
}
