import { Combobox, Input, InputBase, useCombobox } from "@mantine/core";
import type { CSSProperties, KeyboardEvent, SyntheticEvent } from "react";
import "./model-picker.css";

// One catalog row. The tags follow the Codex CLI picker: "(default)" marks the
// catalog default and "(current)" marks the model the chat uses now.
export type ModelOption = {
  value: string;
  label: string;
  description?: string;
  isDefault?: boolean;
  disabled?: boolean;
};

type ModelPickerProps = {
  label: string;
  options: ModelOption[];
  value: string;
  onChange: (value: string) => void;
  id?: string;
  currentValue?: string | null;
  placeholder?: string;
  disabled?: boolean;
  description?: string;
  className?: string;
};

const tagsFor = (option: ModelOption, currentValue?: string | null) => {
  const tags: string[] = [];
  if (option.isDefault) tags.push("default");
  if (currentValue != null && option.value === currentValue)
    tags.push("current");
  return tags;
};
const tagText = (tags: string[]) => (tags.length ? `(${tags.join(", ")})` : "");

// A mousedown inside the portal must not count as a click outside the
// settings popover that owns this picker.
const keepOpen = (event: SyntheticEvent) => event.stopPropagation();

export function ModelPicker({
  label,
  options,
  value,
  onChange,
  id,
  currentValue,
  placeholder = "Select a model",
  disabled = false,
  description,
  className,
}: ModelPickerProps) {
  const combobox = useCombobox({
    onDropdownClose: () => combobox.resetSelectedOption(),
    onDropdownOpen: () =>
      combobox.updateSelectedOptionIndex("active", { scrollIntoView: true }),
  });
  const selected = options.find((option) => option.value === value);
  // The name column is as wide as the longest name so descriptions align.
  const nameWidth = Math.min(
    30,
    Math.max(
      12,
      ...options.map(
        (option) =>
          option.label.length + tagText(tagsFor(option, currentValue)).length,
      ),
    ),
  );
  const onKeyDown = (event: KeyboardEvent<HTMLButtonElement>) => {
    // Escape closes only the open list, not the popover around it.
    if (event.key === "Escape" && combobox.dropdownOpened)
      event.stopPropagation();
  };
  return (
    <Combobox
      store={combobox}
      disabled={disabled}
      width={400}
      position="bottom-start"
      middlewares={{ flip: true, shift: true }}
      onOptionSubmit={(next) => {
        combobox.closeDropdown();
        if (next !== value) onChange(next);
      }}
    >
      <Combobox.Target targetType="button" withExpandedAttribute>
        <InputBase
          component="button"
          type="button"
          id={id}
          className={className}
          label={label}
          description={description}
          pointer
          disabled={disabled}
          data-value={value}
          rightSection={<Combobox.Chevron />}
          rightSectionPointerEvents="none"
          onKeyDown={onKeyDown}
          onClick={() => combobox.toggleDropdown()}
        >
          {selected ? (
            <span className="model-picker-value">
              <span>{selected.label}</span>
              {tagsFor(selected, currentValue).length > 0 && (
                <span className="model-picker-tag">
                  {tagText(tagsFor(selected, currentValue))}
                </span>
              )}
            </span>
          ) : (
            <Input.Placeholder>{placeholder}</Input.Placeholder>
          )}
        </InputBase>
      </Combobox.Target>
      <Combobox.Dropdown
        className="model-picker-dropdown"
        onMouseDown={keepOpen}
        onTouchStart={keepOpen}
      >
        <Combobox.Options
          className="model-picker-options"
          aria-label={`${label} list`}
          style={{ "--model-picker-name": `${nameWidth}ch` } as CSSProperties}
        >
          {options.map((option) => {
            const tags = tagsFor(option, currentValue);
            return (
              <Combobox.Option
                key={option.value}
                value={option.value}
                active={option.value === value}
                disabled={option.disabled}
                aria-disabled={option.disabled || undefined}
                data-value={option.value}
                className="model-picker-row"
              >
                <span className="model-picker-name">
                  {option.label}
                  {tags.length > 0 && (
                    <span className="model-picker-tag"> {tagText(tags)}</span>
                  )}
                </span>
                {option.description && (
                  <span className="model-picker-description">
                    {option.description}
                  </span>
                )}
              </Combobox.Option>
            );
          })}
        </Combobox.Options>
      </Combobox.Dropdown>
    </Combobox>
  );
}
