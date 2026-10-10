import { Textarea as MantineTextarea, type TextareaProps } from "@mantine/core";
import { forwardRef } from "react";

export type AutosizeTextareaProps = TextareaProps;

/** Multiline text field with the Studio composer sizing and visual defaults. */
export const AutosizeTextarea = forwardRef<
  HTMLTextAreaElement,
  AutosizeTextareaProps
>(function AutosizeTextarea(
  { autosize = true, minRows = 1, variant = "unstyled", ...props },
  ref,
) {
  return (
    <MantineTextarea
      {...props}
      ref={ref}
      autosize={autosize}
      minRows={minRows}
      variant={variant}
    />
  );
});
