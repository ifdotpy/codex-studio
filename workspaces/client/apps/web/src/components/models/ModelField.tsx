import { NativeSelect, Radio, Stack } from "@mantine/core";
import {
  INHERITED_MODEL,
  modelOptionLabel,
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
  if (presentation === "select") {
    const selected = options.find((option) => option.value === value);
    return (
      <NativeSelect
        id={id}
        label={label}
        size="sm"
        value={value}
        data-value={value}
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
    <fieldset
      id={id}
      className="model-fieldset"
      disabled={disabled}
      aria-label={label}
      data-value={value}
      aria-disabled={disabled}
    >
      <legend>{label}</legend>
      <Radio.Group value={value} onChange={onChange} size="sm">
        <Stack gap="xs" className="model-options">
          {options.map((option) => (
            <Radio
              key={option.value}
              value={option.value}
              data-value={option.value}
              data-inherited={option.value === INHERITED_MODEL || undefined}
              label={modelOptionLabel(option)}
              description={option.disabledReason || option.description}
              disabled={disabled || !!option.disabledReason}
              className="model-radio"
            />
          ))}
        </Stack>
      </Radio.Group>
    </fieldset>
  );
}
