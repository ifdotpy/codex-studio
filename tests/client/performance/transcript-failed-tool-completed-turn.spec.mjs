import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import {
  browserExecutablePath,
  installApiSchemaFixtureHeaders,
  test,
} from "../playwright.mjs";

test("failed tool limit remains visible in a completed turn @performance", async () => {
  test.fail(
    true,
    "Completed-command filtering hides a failed tool and its account-limit notice",
  );
  test.setTimeout(30_000);
  const root = fileURLToPath(new URL("../../../web/", import.meta.url));
  const require = createRequire(join(root, "package.json"));
  const { chromium } = require("playwright-core");
  const { createFixtureViteServer: createServer } =
    await import("../playwright.mjs");
  const cacheDir = await mkdtemp(join(tmpdir(), "studio-turn-limit-"));
  const entry = join(root, "transcript-audit.tsx");
  const server = await createServer({
    configFile: false,
    root,
    cacheDir,
    optimizeDeps: {
      noDiscovery: true,
      include: [
        "react",
        "react/jsx-runtime",
        "react/jsx-dev-runtime",
        "react-dom",
        "react-dom/client",
        "@mantine/core",
        "marked",
        "dompurify",
      ],
    },
    server: { host: "127.0.0.1", port: 0 },
    plugins: [
      {
        name: "audit",
        configureServer(vite) {
          vite.middlewares.use("/audit", (_request, response) => {
            response.setHeader("Content-Type", "text/html");
            response.end(
              '<div id="root"></div><script type="module" src="/transcript-audit.tsx"></script>',
            );
          });
        },
        resolveId(id) {
          if (id === "/transcript-audit.tsx") return entry;
        },
        load(id) {
          if (id !== entry) return;
          return `import React, { useState } from "react";
import { createRoot } from "react-dom/client";
import { flushSync } from "react-dom";
import { MantineProvider } from "@mantine/core";
import { marked } from "marked";
import DOMPurify from "dompurify";
import TurnHistory from "/src/components/conversation/transcript/TurnHistory.tsx";
import StreamingText from "/src/components/conversation/transcript/StreamingText.tsx";

const initial = [
  { id: "user", role: "user", text: "Run the command", turnId: "complete-turn", turnStatus: "completed" },
  {
    id: "failed-command",
    role: "output",
    title: "Command finished",
    toolStatus: "completed",
    turnId: "complete-turn",
    turnStatus: "completed",
    text: JSON.stringify({ type: "commandExecution", status: "completed", exitCode: 0 }),
  },
  { id: "answer", role: "assistant", text: "The task is complete.", turnId: "complete-turn", turnStatus: "completed", phase: "final_answer" },
];

function App() {
  const [items, setItems] = useState(initial);
  window.failCompletedTurnTool = () => flushSync(() => setItems((old) => old.map((item) =>
    item.id === "failed-command"
      ? { ...item, toolStatus: "failed", text: JSON.stringify({ type: "commandExecution", status: "failed", error: { message: "Usage limit reached", codexErrorInfo: "usageLimitExceeded" } }) }
      : item,
  )));
  return <MantineProvider><TurnHistory items={items} enabled storageKey="completed-turn-limit" renderMessage={(item) => <StreamingText text={item.text} streaming={false} agentId="fixture" />} onJump={() => {}} /></MantineProvider>;
}

createRoot(document.getElementById("root")).render(<App />);`;
        },
      },
    ],
  });
  let browser;
  try {
    await server.listen();
    browser = await chromium.launch({
      headless: true,
      executablePath: browserExecutablePath,
    });
    const page = await browser.newPage();
    page.setDefaultTimeout(5000);
    installApiSchemaFixtureHeaders(page);
    await page.route("**/api/**", (route) => route.fulfill({ json: {} }));
    await page.goto(
      `http://127.0.0.1:${server.httpServer.address().port}/audit`,
    );
    await page.locator('[data-turn="complete-turn"]').waitFor();
    await page.evaluate(() => window.failCompletedTurnTool());
    await page.getByText("Account limit reached", { exact: false }).waitFor();
    assert.match(
      await page.locator('[data-turn="complete-turn"]').innerText(),
      /Account limit reached/,
    );
  } finally {
    await browser?.close().catch(() => {});
    await server.close();
    await rm(cacheDir, { recursive: true, force: true });
  }
});
