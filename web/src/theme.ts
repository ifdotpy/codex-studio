import {
  createTheme,
  Button,
  ActionIcon,
  TextInput,
  NativeSelect,
  Modal,
  Tooltip,
} from "@mantine/core";

export const theme = createTheme({
  primaryColor: "indigo",
  defaultRadius: "md",
  fontFamily:
    'Inter, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif',
  fontFamilyMonospace: '"SFMono-Regular", Consolas, monospace',
  fontSizes: {
    xs: "0.75rem",
    sm: "0.8125rem",
    md: "0.875rem",
    lg: "1rem",
    xl: "1.25rem",
  },
  headings: { fontWeight: "600" },
  colors: {
    dark: [
      "#e9e9ee",
      "#c2c2cd",
      "#9696a5",
      "#656573",
      "#3a3a43",
      "#2c2c34",
      "#232329",
      "#1b1b20",
      "#151519",
      "#101014",
    ],
  },
  components: {
    Button: Button.extend({
      defaultProps: { size: "sm", variant: "subtle", color: "gray", fw: 500 },
    }),
    ActionIcon: ActionIcon.extend({
      defaultProps: { size: "lg", variant: "subtle", color: "gray" },
    }),
    TextInput: TextInput.extend({ defaultProps: { size: "sm" } }),
    NativeSelect: NativeSelect.extend({ defaultProps: { size: "sm" } }),
    Modal: Modal.extend({
      defaultProps: {
        centered: true,
        radius: "lg",
        padding: "lg",
        overlayProps: { backgroundOpacity: 0.6, blur: 4 },
        closeButtonProps: { "aria-label": "Close" },
      },
    }),
    Tooltip: Tooltip.extend({
      defaultProps: { openDelay: 500, withArrow: true },
    }),
  },
});
