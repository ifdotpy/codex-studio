import { test } from "../playwright.ts";
import { fixtureOrigin } from "./performance-fixture.ts";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { strict as assert } from "node:assert";

for (const variant of ["window", "content-visibility"]) {
  test(`history scrolling comparison: ${variant} @performance`, async ({
    browser,
  }) => {
    test.setTimeout(120_000);
    const { createServer } = await import("vite");
    const cacheDir = await mkdtemp(join(tmpdir(), "studio-scroll-comparison-"));
    const root = fileURLToPath(new URL("../..", import.meta.url));
    const entry = fileURLToPath(
      new URL("./history-window.fixture.tsx", import.meta.url),
    );
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
          name: "history-comparison",
          enforce: "pre",
          transform(code, id) {
            if (id === entry)
              return {
                code: code.replace(
                  "return saved || make(0, 1000)",
                  "return saved || make(0, 100)",
                ),
                map: null,
              };
            if (
              variant === "content-visibility" &&
              id.endsWith("/transcript/HistoryWindow.tsx")
            )
              return {
                code: `import React from "react";
export default function HistoryWindow({rows, render}) { return <div>{rows.map(row => <div key={row.id} data-history-row={row.id} style={{contentVisibility:"auto", containIntrinsicSize:"auto 300px"}}>{render(row)}</div>)}</div>; }`,
                map: null,
              };
          },
          configureServer(s) {
            s.middlewares.use("/audit", (_q, r) => {
              r.setHeader("Content-Type", "text/html");
              r.end(
                '<div id="root"></div><script type="module" src="/history-comparison.tsx"></script>',
              );
            });
          },
          resolveId(id) {
            if (id === "/history-comparison.tsx") return entry;
          },
        },
      ],
    });
    const page = await browser.newPage({
      viewport: { width: 1100, height: 900 },
    });
    try {
      await page.addInitScript(() => {
        (window as typeof window & { longTasks: number[] }).longTasks = [];
        new PerformanceObserver((list) =>
          (window as typeof window & { longTasks: number[] }).longTasks.push(
            ...list.getEntries().map((e) => e.duration),
          ),
        ).observe({ type: "longtask", buffered: true });
      });
      await server.listen();
      const start = Date.now();
      await page.goto(`${fixtureOrigin(server)}/audit`);
      await page.locator('[data-message="a99"]').waitFor({ state: "attached" });
      const initialMs = Date.now() - start;
      await page.waitForTimeout(500);
      const metrics = await page.evaluate(async () => {
        const root = document.querySelector<HTMLElement>("#messages")!;
        let writes = 0;
        const setItem = Storage.prototype.setItem;
        Storage.prototype.setItem = function (...args) {
          writes++;
          return setItem.apply(this, args);
        };
        const before = (window as typeof window & { renderedMessages: number })
          .renderedMessages;
        const longTasks = (window as typeof window & { longTasks: number[] })
          .longTasks.length;
        const frames: number[] = [];
        for (let i = 0; i < 20; i++) {
          const start = performance.now();
          root.scrollTop -= 100;
          root.dispatchEvent(new Event("scroll"));
          await new Promise<void>((resolve) =>
            requestAnimationFrame(() => requestAnimationFrame(() => resolve())),
          );
          frames.push(performance.now() - start);
        }
        return {
          frames,
          writes,
          renders:
            (window as typeof window & { renderedMessages: number })
              .renderedMessages - before,
          longTasks:
            (window as typeof window & { longTasks: number[] }).longTasks
              .length - longTasks,
          nodes: document.querySelectorAll("[data-message]").length,
        };
      });
      console.log(JSON.stringify({ variant, initialMs, ...metrics }));
      assert.equal(metrics.writes, 0);
    } finally {
      await page.close();
      await server.close();
      await rm(cacheDir, { recursive: true, force: true });
    }
  });
}
