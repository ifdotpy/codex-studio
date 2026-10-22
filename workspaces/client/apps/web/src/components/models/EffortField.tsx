import { NativeSelect } from "@mantine/core";

type Props = {
  label: string;
  options: { value: string; label: string }[];
  value: string;
  onChange: (value: string) => void;
  disabled?: boolean;
};

/** Native select keeps even the longest provider effort set usable at phone width. */
export function EffortField({
  label,
  options,
  value,
  onChange,
  disabled,
}: Props) {
  return options.length ? (
    <NativeSelect
      label={label}
      aria-label={label}
      size="sm"
      value={value}
      data-value={value}
      disabled={disabled}
      data={options}
      onChange={(event) => onChange(event.currentTarget.value)}
    />
  ) : null;
}
