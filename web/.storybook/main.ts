import type { StorybookConfig } from "@storybook/react-vite";

const config: StorybookConfig = {
  stories: ["../src/**/*.stories.@(ts|tsx|mdx)"],
  addons: ["@storybook/addon-vitest"],
  framework: "@storybook/react-vite",
  core: {
    disableTelemetry: true,
  },
};

export default config;
