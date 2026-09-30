import type { Preview } from "@storybook/react-vite";
import { MantineProvider } from "@mantine/core";
import "@mantine/core/styles.css";
import "../src/style.css";
import "../src/studio-theme.css";
import "../src/components/chat-controls.css";
import { theme } from "../src/theme";

const preview: Preview = {
  decorators: [
    (Story) => (
      <MantineProvider theme={theme} defaultColorScheme="dark">
        <div style={{ color: "var(--text)", padding: 24 }}>
          <Story />
        </div>
      </MantineProvider>
    ),
  ],
  parameters: {
    layout: "centered",
  },
};

export default preview;
