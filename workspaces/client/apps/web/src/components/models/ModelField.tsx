import { Button, NativeSelect, Text } from "@mantine/core";
import {
  INHERITED_MODEL,
  modelOptionLabel,
  modelForValue,
  type ModelOption,
} from "./modelOption";
import "./model-field.css";
import { useModelWhitelist, whitelistedModelOptions } from "./modelWhitelist";

type Props = {
  label: string;
  options: ModelOption[];
  value: string;
  onChange: (value: string) => void;
  presentation?: "list" | "select";
  id?: string;
  disabled?: boolean;
  retainCurrent?: boolean;
};

export function ModelField({
  label,
  options: allOptions,
  value,
  onChange,
  presentation = "list",
  id,
  disabled,
  retainCurrent = true,
}: Props) {
  const allowed = useModelWhitelist();
  const options = whitelistedModelOptions(
    allOptions,
    value,
    allowed,
    retainCurrent,
  );
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
    <div className="model-field">
      <Text size="sm" fw={500}>
        {label}
      </Text>
      <div
        id={id}
        className="setup-models"
        role="listbox"
        aria-label={label}
        aria-disabled={disabled || undefined}
        data-value={selectedValue}
        tabIndex={0}
        onKeyDown={(event) => {
          if (!["ArrowDown", "ArrowUp", "Home", "End"].includes(event.key))
            return;
          event.preventDefault();
          const rows = Array.from(
            event.currentTarget.querySelectorAll<HTMLButtonElement>(
              "button:not(:disabled)",
            ),
          );
          const focused = rows.indexOf(
            document.activeElement as HTMLButtonElement,
          );
          const index =
            event.key === "Home"
              ? 0
              : event.key === "End"
                ? rows.length - 1
                : (focused +
                    (event.key === "ArrowDown" ? 1 : -1) +
                    rows.length) %
                  rows.length;
          rows[index]?.focus();
        }}
      >
        {options.map((option) => (
          <Button
            key={option.value}
            variant="subtle"
            role="option"
            aria-selected={selectedValue === option.value}
            aria-disabled={disabled || !!option.disabledReason}
            aria-description={option.disabledReason}
            data-value={option.value}
            data-inherited={option.value === INHERITED_MODEL || undefined}
            data-selected={selectedValue === option.value || undefined}
            disabled={disabled || !!option.disabledReason}
            title={option.disabledReason || option.description}
            onClick={() => onChange(option.value)}
          >
            <span className="setup-model-name">{option.label}</span>
            {option.badges.length > 0 && (
              <small>
                {option.badges
                  .map((badge) => (badge === "default" ? "recommended" : badge))
                  .join(", ")}
              </small>
            )}
          </Button>
        ))}
      </div>
      {selected?.description && (
        <Text
          size="xs"
          c="dimmed"
          className="model-description"
          title={selected.description}
        >
          {selected.description}
        </Text>
      )}
      {[
        ...new Set(
          options.map((option) => option.disabledReason).filter(Boolean),
        ),
      ].map((reason) => (
        <Text
          size="xs"
          c="dimmed"
          key={reason}
          className="model-unavailable-reason"
        >
          {reason}
        </Text>
      ))}
    </div>
  );
}
