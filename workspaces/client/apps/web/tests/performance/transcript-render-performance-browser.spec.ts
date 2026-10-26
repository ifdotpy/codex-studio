import { fixtureOrigin } from "./performance-fixture.ts";
import type {} from "./transcript-render-performance.fixture.tsx";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  test,
  browserExecutablePath,
  installApiSchemaFixtureHeaders,
} from "../playwright.ts";

test("Transcript render performance @performance", async () => {
  test.setTimeout(180_000);
  const root = fileURLToPath(new URL("../..//", import.meta.url));
  const { chromium, webkit } = await import("playwright-core");
  const browserType = process.env.BROWSER === "webkit" ? webkit : chromium;
  const { createFixtureViteServer: createServer } =
    await import("../playwright.ts");
  const cacheDir = await mkdtemp(join(tmpdir(), "studio-render-audit-"));
  const entry = new URL(
    "./transcript-render-performance.fixture.tsx",
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
        configureServer(s) {
          s.middlewares.use("/audit", (_q, r) => {
            r.setHeader("Content-Type", "text/html");
            r.end(
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
    browser = await browserType.launch({
      headless: true,
      ...(browserType === chromium
        ? {
            executablePath: browserExecutablePath,
          }
        : {}),
    });
    const page = await browser.newPage();
    const errors: string[] = [];
    page.on("pageerror", (e) => {
      errors.push(e.message);
      console.error("Browser error:", e.message);
    });
    installApiSchemaFixtureHeaders(page);
    await page.route("**/api/**", (r) => r.fulfill({ json: {} }));
    await page.goto(fixtureOrigin(server) + "/audit");
    await page.waitForSelector(".turn-history");
    await page.waitForTimeout(250);
    const reset = () =>
      page.evaluate(() =>
        Object.assign(
          window.transcriptPerformanceCounters,
          Object.fromEntries(
            Object.keys(window.transcriptPerformanceCounters).map((k) => [
              k,
              0,
            ]),
          ),
        ),
      );
    await page
      .locator(".turn-history")
      .first()
      .evaluate((el) => (window.transcriptPerformanceOriginalTurn = el));
    await reset();
    await page.evaluate(() => {
      for (let i = 0; i < 10; i++) window.transcriptPerformanceTick();
    });
    const unchanged = await page.evaluate(() => ({
      ...window.transcriptPerformanceCounters,
    }));
    assert.equal(
      unchanged.lex,
      0,
      "Completed replies do not reparse on parent updates",
    );
    assert.equal(
      unchanged.toolParse,
      0,
      "Unchanged tool diagnostics do not reparse",
    );
    assert.equal(unchanged.sanitize, 0);
    await page.evaluate(() => window.transcriptPerformanceReplaceLast());
    await page.getByRole("button", { name: "Updated report" }).waitFor();
    const changed = await page.evaluate(() => ({
      ...window.transcriptPerformanceCounters,
    }));
    assert.equal(
      changed.lex,
      2,
      "Only the changed reply and its results reparse",
    );
    assert.equal(
      await page
        .locator(".turn-history")
        .first()
        .evaluate((el) => el === window.transcriptPerformanceOriginalTurn),
      true,
      "Earlier turn retains its DOM node",
    );
    await page.evaluate(() =>
      window.transcriptPerformanceDiagnostic({
        message: "First diagnostic",
        code: "first",
      }),
    );
    await page
      .getByText("First diagnostic", { exact: false })
      .first()
      .waitFor();
    await page.evaluate(() =>
      window.transcriptPerformanceDiagnostic({
        message: "Updated diagnostic",
        code: "second",
      }),
    );
    await page
      .getByText("Updated diagnostic", { exact: false })
      .first()
      .waitFor();
    assert.equal(
      await page.getByText("First diagnostic", { exact: false }).count(),
      0,
    );
    await page.evaluate(() => window.transcriptPerformanceMode("stream"));
    await reset();
    await page.evaluate(() => {
      for (let i = 0; i < 20; i++) window.transcriptPerformanceAppend();
    });
    const streaming = await page.evaluate(() => ({
      ...window.transcriptPerformanceCounters,
    }));
    assert.equal(
      streaming.lex,
      20,
      "Each stream update still parses full Markdown",
    );
    assert.equal(
      streaming.sanitize,
      20,
      "Only changed final HTML requires sanitizing",
    );
    await page.evaluate(() =>
      window.transcriptPerformanceStream(
        "Read [report][ref].\n\nStable paragraph.",
      ),
    );
    await page
      .getByText("Stable paragraph.", { exact: true })
      .evaluate((el) => (window.transcriptPerformanceOriginalSentence = el));
    await page.evaluate(() =>
      window.transcriptPerformanceStream(
        "Read [report][ref].\n\nStable paragraph.\n\n[ref]: ./first.md",
      ),
    );
    assert.equal(
      await page.getByRole("link", { name: "report" }).getAttribute("href"),
      "./first.md",
    );
    assert.equal(
      await page
        .getByText("Stable paragraph.", { exact: true })
        .evaluate((el) => el === window.transcriptPerformanceOriginalSentence),
      true,
    );
    await page.evaluate(() =>
      window.transcriptPerformanceStream(
        "Read [report][ref].\n\nStable paragraph.\n\n[ref]: ./second.md",
      ),
    );
    assert.equal(
      await page.getByRole("link", { name: "report" }).getAttribute("href"),
      "./second.md",
      "Later reference definition updates earlier block",
    );
    await page.evaluate(() =>
      window.transcriptPerformanceStream("```ts\nconst x = 1;"),
    );
    await page.locator(".prose pre").waitFor();
    await page.evaluate(() =>
      window.transcriptPerformanceStream(
        "```ts\nconst x = 1;\n```\n\nAfter code.",
      ),
    );
    await page.getByText("After code.", { exact: true }).waitFor();
    assert.equal(await page.locator(".prose pre").count(), 1);
    assert.deepEqual(errors, []);
    console.log(
      JSON.stringify({
        browser: browserType.name(),
        unchanged,
        changed,
        streaming,
      }),
    );
    console.log(
      "PASS transcript parsing cache, changed turns, native diagnostics, reference links, open fences and retained DOM",
    );
  } finally {
    await browser?.close();
    await server.close();
    await rm(cacheDir, { recursive: true, force: true });
  }
});

declare global {
  interface Window {
    transcriptPerformanceOriginalTurn: Element;
    transcriptPerformanceOriginalSentence: Element;
  }
}
