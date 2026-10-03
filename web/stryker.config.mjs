/** @type {import('@stryker-mutator/api/core').PartialStrykerOptions} */
const config = {
  plugins: ["@stryker-mutator/vitest-runner"],
  testRunner: "vitest",
  vitest: {
    configFile: "vitest.unit.config.ts",
    related: true,
  },
  mutate: [
    "src/usage/tokenRate.ts",
    "src/usage/weeklyRunway.ts",
    "src/usage/accountUsage.ts",
    "src/conversation/transcriptIdentity.ts",
    "src/sync/entityProjection.ts",
    "src/components/chat-status/chatStatusModel.ts",
    "src/components/conversation-results/conversationResultModel.ts",
    "src/components/file-change/fileChangeModel.ts",
    "src/components/message-delivery/messageDelivery.ts",
    "src/components/sentence-stream/sentenceStream.ts",
    "src/components/tool-images/toolImages.ts",
    "src/desktop/desktopAlerts.ts",
  ],
  concurrency: 2,
  coverageAnalysis: "perTest",
  reporters: ["clear-text", "progress", "html", "json"],
  htmlReporter: {
    fileName: "reports/mutation/stryker.html",
  },
  jsonReporter: {
    fileName: "reports/mutation/stryker.json",
  },
};

export default config;
