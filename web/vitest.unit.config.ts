import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    name: "unit",
    root: ".",
    environment: "node",
    include: ["src/**/*.test.ts", "src/**/*.test.mjs"],
    // This legacy browser harness is still run by test:prompt-composer.
    exclude: ["src/components/prompt-composer/PromptInput.test.mjs"],
  },
});
