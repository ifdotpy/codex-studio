import type { Preview } from "@storybook/react-vite";
import { MantineProvider } from "@mantine/core";
import "@mantine/core/styles.css";
import "../src/style.css";
import "../src/workspace-layout.css";
import "../src/appearance.css";
import "../src/studio-preferences.css";
import "../src/design/state.css";
import "../src/components/team-navigation.css";
import "../src/components/chat-controls.css";
import { theme } from "../src/theme";

const preview: Preview = {
  decorators: [
    (Story, context) => (
      <MantineProvider
        theme={theme}
        forceColorScheme={
          context.globals.colorScheme === "light" ? "light" : "dark"
        }
      >
        <div
          style={{
            color: "var(--text)",
            padding: "var(--mantine-spacing-sm)",
            minWidth: 0,
          }}
        >
          <Story />
        </div>
      </MantineProvider>
    ),
  ],
  initialGlobals: { colorScheme: "dark" },
  globalTypes: {
    colorScheme: {
      toolbar: { icon: "circlehollow", items: ["light", "dark"] },
    },
  },
  parameters: {
    layout: "centered",
  },
};

export default preview;
