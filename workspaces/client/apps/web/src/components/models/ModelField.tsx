import { NativeSelect, Radio, Text } from "@mantine/core";
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
};

const contextVariant =
  /\s*(?:with\s+)?[([]?(\d+(?:\.\d+)?[KM])(?:\s+context)?[)\]]?$/i;
const contextFor = (option: ModelOption) =>
  (
    option.label.match(contextVariant)?.[1] ||
    option.value.match(/\[(\d+(?:\.\d+)?[KM])\]$/i)?.[1]
  )?.toUpperCase();
const choiceLabel = (option: ModelOption) =>
  option.label.replace(contextVariant, "").trim();
const groupFor = (option: ModelOption) => {
  if (option.provider === "claude")
    return /^(Opus|Sonnet|Haiku)\b/i.exec(choiceLabel(option))?.[1] || "Claude";
  if (option.provider === "codex") {
    const generation = /\b(\d+)(?:\.\d+)?$/.exec(choiceLabel(option))?.[1];
    return generation ? `Codex ${generation}.x` : "Codex";
  }
  return option.provider;
};
const versionFor = (option: ModelOption) =>
  /\b(\d+(?:\.\d+)?)$/.exec(choiceLabel(option))?.[1] || "";

export function ModelField({
  label,
  options: allOptions,
  value,
  onChange,
  presentation = "list",
  id,
  disabled,
}: Props) {
  const allowed = useModelWhitelist();
  const options = whitelistedModelOptions(allOptions, value, allowed);
  const selected = modelForValue(options, value);
  const selectedValue = selected?.value ?? value;
  const providers = [...new Set(options.map((option) => option.provider))];
  const firstModel =
    options.find((option) => option.value !== INHERITED_MODEL) || options[0];
  const firstGroup = firstModel ? groupFor(firstModel) : "";
  const optionGroup = (option: ModelOption) =>
    option.value === INHERITED_MODEL ? firstGroup : groupFor(option);
  const groups = [...new Set(options.map(optionGroup))].sort((a, b) => {
    const family = ["Opus", "Sonnet", "Haiku", "Claude"];
    if (family.includes(a) && family.includes(b))
      return family.indexOf(a) - family.indexOf(b);
    const aProvider =
      options.find((option) => optionGroup(option) === a)?.provider || "";
    const bProvider =
      options.find((option) => optionGroup(option) === b)?.provider || "";
    return (
      providers.indexOf(aProvider) - providers.indexOf(bProvider) ||
      b.localeCompare(a, undefined, { numeric: true })
    );
  });
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
      {groups.map((group) => (
        <div key={group} className="model-family-group">
          {(groups.length > 1 ||
            !options.every((option) => option.value === INHERITED_MODEL)) && (
            <Text size="xs" c="dimmed" className="model-family-label">
              {group}
            </Text>
          )}
          <div className="model-options">
            {options
              .filter((option) => optionGroup(option) === group)
              .sort((a, b) =>
                a.value === INHERITED_MODEL
                  ? -1
                  : b.value === INHERITED_MODEL
                    ? 1
                    : versionFor(b).localeCompare(versionFor(a), undefined, {
                        numeric: true,
                      }),
              )
              .map((option) => (
                <Radio
                  key={option.value}
                  value={option.value}
                  data-value={option.value}
                  data-unavailable={!!option.disabledReason || undefined}
                  data-inherited={option.value === INHERITED_MODEL || undefined}
                  label={
                    <span className="model-option-label">
                      <span className="model-option-name">
                        {choiceLabel(option)}
                      </span>
                      {contextFor(option) && (
                        <small className="model-context-badge">
                          {contextFor(option)}
                        </small>
                      )}
                      {option.badges.length > 0 && (
                        <small className="model-option-badges">
                          {" "}
                          ({option.badges.join(", ")})
                        </small>
                      )}
                    </span>
                  }
                  aria-description={option.disabledReason}
                  title={option.disabledReason || option.description}
                  disabled={disabled || !!option.disabledReason}
                  className="model-radio"
                />
              ))}
          </div>
        </div>
      ))}
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
    </Radio.Group>
  );
}
