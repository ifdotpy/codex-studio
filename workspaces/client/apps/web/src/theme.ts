import {
  createTheme,
  defaultVariantColorsResolver,
  Button,
  ActionIcon,
  TextInput,
  NativeSelect,
  Modal,
  Tooltip,
  Menu,
} from "@mantine/core";

// Mantine defaults put popovers at 300 and overlays at 400. Dialogs opened
// from a popover sit between them so they own focus and input.
export const layerZIndex = {
  app: 100,
  modal: 200,
  popover: 300,
  modalAbovePopover: 350,
  overlay: 400,
  max: 9999,
} as const;

export const modalSizes = { settings: 720 };

export const theme = createTheme({
  primaryColor: "signal",
  primaryShade: { light: 7, dark: 4 },
  autoContrast: true,
  // Mantine's component resolver chooses text using the light primary shade.
  // The generated contrast variable follows both primaryShade schemes.
  variantColorResolver: (input) => {
    // Uncolored filled actions use Signal; the default subtle action stays neutral.
    const colors = defaultVariantColorsResolver(
      input.variant === "subtle" && input.color === input.theme.primaryColor
        ? { ...input, color: "gray" }
        : input,
    );
    return input.variant === "filled" &&
      input.color === input.theme.primaryColor
      ? { ...colors, color: "var(--mantine-primary-color-contrast)" }
      : colors;
  },
  defaultRadius: "md",
  fontFamily:
    'Inter, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif',
  fontFamilyMonospace: '"SFMono-Regular", Consolas, monospace',
  fontSizes: {
    xs: "var(--studio-text-meta)",
    sm: "var(--studio-text-body)",
    md: "var(--studio-text-body)",
    lg: "var(--studio-text-title)",
    xl: "var(--studio-text-section)",
  },
  headings: { fontWeight: "600" },
  colors: {
    // Ten steps from pale signal to deep teal; primary shade depends on scheme.
    signal: [
      "#e7f8f8",
      "#c9eeee",
      "#9bdfdf",
      "#6dd0d2",
      "#3fc1c3",
      "#20a6a9",
      "#0e8c8f",
      "#08777a",
      "#09666a",
      "#095256",
    ],
    // Dark[6] owns panel Surface; dark[7] owns page Graphite.
    dark: [
      "#e6eaf0",
      "#bdc6d2",
      "#95a1b0",
      "#6c7a8c",
      "#3b4858",
      "#27323e",
      "#1a2028",
      "#12161c",
      "#0e1217",
      "#0a0e13",
    ],
  },
  components: {
    Button: Button.extend({
      styles: {
        label: { textBoxTrim: "none", height: "auto", lineHeight: 1.4 },
        inner: { transform: "none", transition: "opacity 100ms ease" },
      },
      defaultProps: { size: "sm", variant: "subtle", fw: 500 },
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
      styles: { dropdown: { minWidth: "var(--studio-menu-min)" } },
    }),
    Tooltip: Tooltip.extend({
      defaultProps: { openDelay: 500, withArrow: true },
    }),
  },
});
