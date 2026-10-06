import {
  createTheme,
  Button,
  ActionIcon,
  TextInput,
  NativeSelect,
  Modal,
  Tooltip,
  Menu,
} from "@mantine/core";

export const modalSizes = { settings: 720 };

export const theme = createTheme({
  primaryColor: "indigo",
  defaultRadius: "md",
  fontFamily:
    'Inter, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif',
  fontFamilyMonospace: '"SFMono-Regular", Consolas, monospace',
  fontSizes: {
    xs: "var(--studio-text-meta)",
    sm: "var(--studio-text-body)",
    md: "var(--studio-text-body)",
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
      styles: {
        inner: { transform: "none", transition: "opacity 100ms ease" },
      },
      defaultProps: { size: "sm", variant: "subtle", color: "gray", fw: 500 },
    }),
    ActionIcon: ActionIcon.extend({
      defaultProps: {
        size: "var(--studio-action-size)",
        variant: "subtle",
        color: "gray",
      },
    }),
    TextInput: TextInput.extend({ defaultProps: { size: "sm" } }),
    NativeSelect: NativeSelect.extend({ defaultProps: { size: "sm" } }),
    Modal: Modal.extend({
      defaultProps: {
        centered: true,
        radius: "lg",
        padding: "lg",
        overlayProps: { backgroundOpacity: 0.6, blur: 1 },
        closeButtonProps: { "aria-label": "Close" },
      },
    }),
    Menu: Menu.extend({
      styles: { dropdown: { minWidth: 220 } },
    }),
    Tooltip: Tooltip.extend({
      defaultProps: { openDelay: 500, withArrow: true },
    }),
  },
});
