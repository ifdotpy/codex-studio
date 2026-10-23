import { SegmentedControl } from "@mantine/core";

type Props = {
  label: string;
  options: { value: string; label: string }[];
  value: string;
  onChange: (value: string) => void;
  disabled?: boolean;
};

/** An ordinal native radio scale, with no width-dependent JavaScript. */
export function EffortField({
  label,
  options,
  value,
  onChange,
  disabled,
}: Props) {
  return options.length ? (
    <SegmentedControl
      className="effort-scale"
      fullWidth
      size="xs"
      value={value}
      data-value={value}
      aria-label={label}
      disabled={disabled}
      aria-disabled={disabled}
      onChange={onChange}
      data={options.map((option) => ({
        ...option,
        label: option.label.charAt(0).toUpperCase() + option.label.slice(1),
      }))}
    />
  ) : null;
}
