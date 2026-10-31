import type { StorybookConfig } from "@storybook/react-vite";

const config: StorybookConfig = {
  stories: ["../src/**/*.stories.@(ts|tsx|mdx)"],
  addons: ["@storybook/addon-vitest"],
  framework: "@storybook/react-vite",
  core: {
    disableTelemetry: true,
  },
  viteFinal: (config) => ({
    ...config,
    optimizeDeps: {
      ...config.optimizeDeps,
      include: [
        ...new Set([
          ...(config.optimizeDeps?.include || []),
          "dompurify",
          "lucide-react",
          "mermaid",
        ]),
      ],
    },
  }),
};

export default config;
