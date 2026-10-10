import {
  ActionIcon as MantineActionIcon,
  type ActionIconProps,
  type ElementProps,
} from "@mantine/core";

export interface IconActionProps extends Omit<
  ActionIconProps & ElementProps<"button">,
  "aria-label" | "title"
> {
  label: string;
  title?: string;
}

/** An icon-only action always has a visible-to-assistive-technology name. */
export function IconAction({
  label,
  title = label,
  ...props
}: IconActionProps) {
  return <MantineActionIcon {...props} aria-label={label} title={title} />;
}
