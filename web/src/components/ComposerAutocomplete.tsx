import { Combobox, useCombobox } from "@mantine/core";
import {
  cloneElement,
  useEffect,
  useState,
  type KeyboardEvent,
  type ReactElement,
} from "react";

interface Option {
  value: string;
  label: string;
  description?: string;
}

/** Mantine owns option selection, scrolling, popup positioning and mouse focus.
 * This adapter keeps textarea send/queue shortcuts outside the autocomplete. */
export default function ComposerAutocomplete({
  children,
  opened,
  options,
  loading,
  error,
  warning,
  id,
  label,
  resetKey,
  loadingMessage = "Loading…",
  emptyMessage = "No matches",
  onSelect,
  onDismiss,
}: {
  children: ReactElement<{
    onKeyDown?: (event: KeyboardEvent<HTMLTextAreaElement>) => void;
  }>;
  opened: boolean;
  options: Option[];
  loading: boolean;
  error?: string;
  warning?: string;
  id: string;
  label: string;
  resetKey?: string;
  loadingMessage?: string;
  emptyMessage?: string;
  onSelect: (value: string) => void;
  onDismiss: () => void;
}) {
  const store = useCombobox({ opened, onDropdownClose: onDismiss });
  const [activeId, setActiveId] = useState<string | null>(null);
  const optionKey = JSON.stringify(options.map((option) => option.value));
  const selectable = opened && !loading && !error && options.length > 0;
  const { resetSelectedOption, selectFirstOption } = store;
  useEffect(() => {
    resetSelectedOption();
    setActiveId(selectable ? selectFirstOption() : null);
  }, [
    opened,
    resetKey,
    optionKey,
    selectable,
    resetSelectedOption,
    selectFirstOption,
  ]);

  const onKeyDown = (event: KeyboardEvent<HTMLTextAreaElement>) => {
    if (
      opened &&
      !event.nativeEvent.isComposing &&
      event.keyCode !== 229 &&
      !event.shiftKey &&
      !event.altKey &&
      !event.ctrlKey &&
      !event.metaKey
    ) {
      if (event.key === "Escape") {
        event.preventDefault();
        onDismiss();
        return;
      }
      if (
        selectable &&
        (event.key === "ArrowDown" || event.key === "ArrowUp")
      ) {
        event.preventDefault();
        setActiveId(
          event.key === "ArrowDown"
            ? store.selectNextOption()
            : store.selectPreviousOption(),
        );
        return;
      }
      if (event.key === "Enter" || event.key === "Tab") {
        if (loading || selectable) {
          event.preventDefault();
          if (selectable) store.clickSelectedOption();
          return;
        }
      }
    }
    children.props.onKeyDown?.(event);
  };

  return (
    <Combobox
      store={store}
      onOptionSubmit={onSelect}
      position="top-start"
      keepMounted={false}
      withinPortal
      shadow="sm"
      offset={6}
    >
      <Combobox.Target
        withKeyboardNavigation={false}
        withAriaAttributes={false}
      >
        {cloneElement(children, {
          onKeyDown,
          ...{
            role: "combobox",
            "aria-autocomplete": "list" as const,
            "aria-controls": opened ? id : undefined,
            "aria-expanded": opened,
            "aria-haspopup": "listbox" as const,
            "aria-activedescendant": opened ? activeId || undefined : undefined,
          },
        })}
      </Combobox.Target>
      <Combobox.Dropdown>
        <Combobox.Options
          id={id}
          aria-label={label}
          className="composer-autocomplete-options"
        >
          {loading ? (
            <Combobox.Empty role="status">{loadingMessage}</Combobox.Empty>
          ) : error ? (
            <Combobox.Empty role="status">{error}</Combobox.Empty>
          ) : options.length ? (
            options.map((option) => (
              <Combobox.Option
                key={option.value}
                value={option.value}
                className="composer-autocomplete-option"
              >
                <span>{option.label}</span>
                {option.description && <small>{option.description}</small>}
              </Combobox.Option>
            ))
          ) : (
            <Combobox.Empty role="status">{emptyMessage}</Combobox.Empty>
          )}
          {warning && !error && (
            <Combobox.Empty role="status">{warning}</Combobox.Empty>
          )}
        </Combobox.Options>
      </Combobox.Dropdown>
    </Combobox>
  );
}
