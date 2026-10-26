import {
  ActionIcon as MantineActionIcon,
  type ActionIconProps,
  type ElementProps,
} from "@mantine/core";

export interface IconActionProps extends Omit<
  ActionIconProps & ElementProps<"button">,
  "aria-label" | "title" | "color"
> {
  label: string;
  color?: ActionIconProps["color"] | undefined;
  title?: string;
}

/** An icon-only action always has a visible-to-assistive-technology name. */
export function IconAction({
  label,
  title = label,
  color,
  ...props
}: IconActionProps) {
  return (
    <MantineActionIcon
      {...props}
      {...(color === undefined ? {} : { color })}
      aria-label={label}
      title={title}
    />
  );
}
