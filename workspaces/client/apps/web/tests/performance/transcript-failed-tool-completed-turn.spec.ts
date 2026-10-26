import { fixtureOrigin } from "./performance-fixture.ts";
import type {} from "./transcript-failed-tool-completed-turn.fixture.tsx";
import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import {
  browserExecutablePath,
  installApiSchemaFixtureHeaders,
  test,
} from "../playwright.ts";

test("failed tool limit remains visible in a completed turn @performance", async () => {
  test.fail(
    true,
    "Completed-command filtering hides a failed tool and its account-limit notice",
  );
  test.setTimeout(30_000);
  const root = fileURLToPath(new URL("../../", import.meta.url));
  const { chromium } = await import("playwright-core");
  const { createFixtureViteServer: createServer } =
    await import("../playwright.ts");
  const cacheDir = await mkdtemp(join(tmpdir(), "studio-turn-limit-"));
  const entry = new URL(
    "./transcript-failed-tool-completed-turn.fixture.tsx",
    import.meta.url,
  ).pathname;
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
    await page.goto(`${fixtureOrigin(server)}/audit`);
    await page.locator('[data-turn="complete-turn"]').waitFor();
    await page.evaluate(() => window.failedToolFailCompletedTurnTool());
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
