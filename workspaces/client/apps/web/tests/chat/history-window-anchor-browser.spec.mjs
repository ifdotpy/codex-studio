import { createRequire } from "node:module";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { expect, test } from "../playwright.mjs";

test("forward paging keeps the reading row mounted and at its viewport offset", async ({
  browser,
}) => {
  test.setTimeout(90_000);
  const root = fileURLToPath(new URL("../..", import.meta.url));
  const require = createRequire(join(root, "package.json"));
  const cacheDir = await mkdtemp(join(tmpdir(), "history-window-anchor-vite-"));
  const entry = join(root, "history-window-anchor-audit.tsx");
  const { createServer } = await import(require.resolve("vite"));
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
      ],
    },
    server: { host: "127.0.0.1", port: 0 },
    plugins: [
      {
        name: "history-window-anchor-audit",
        configureServer(vite) {
          vite.middlewares.use("/audit", (_request, response) => {
            response.setHeader("Content-Type", "text/html");
            response.end(
              '<div id="root"></div><script type="module" src="/history-window-anchor-audit.tsx"></script>',
            );
          });
        },
        resolveId(id) {
          if (id === "/history-window-anchor-audit.tsx") return entry;
        },
        load(id) {
          if (id !== entry) return;
          return `import React, { useRef, useState } from "react";
import { createRoot } from "react-dom/client";
import { flushSync } from "react-dom";
import HistoryWindow from "/src/components/conversation/transcript/HistoryWindow.tsx";
const makeRows = (start, count) => Array.from({length: count}, (_, n) => {
  const id = "message-" + (start + n);
  return {id, items: [{id, role: "user", text: "A short fixed-height transcript row", turnId: id}]};
});
function Harness() {
  const [rows, setRows] = useState(() => makeRows(0, 160));
  const scroll = useRef(null);
  window.appendForwardPage = () => {
    const root = scroll.current;
    const holdScroll = event => event.stopImmediatePropagation();
    root.addEventListener("scroll", holdScroll, true);
    flushSync(() => setRows(current => [...current, ...makeRows(160, 60)]));
    root.scrollTop = root.scrollHeight;
    window.releaseScroll = () => root.removeEventListener("scroll", holdScroll, true);
  };
  return <div id="messages" ref={scroll} style={{height: 500, overflow: "auto", position: "relative"}}>
    <HistoryWindow rows={rows} scrollContainer={scroll} storageKey="history-window-anchor-audit" render={row => <article data-message={row.items[0].id} style={{height: 72, minHeight: 72, boxSizing: "border-box"}}>{row.items[0].text}</article>} />
  </div>;
}
createRoot(document.getElementById("root")).render(<Harness/>);`;
        },
      },
    ],
  });
  const context = await browser.newContext({
    viewport: { width: 900, height: 700 },
  });
  const page = await context.newPage();
  try {
    await server.listen();
    await page.goto(
      `http://127.0.0.1:${server.httpServer.address().port}/audit`,
    );
    const anchor = page.locator('[data-message="message-20"]');
    const rootNode = page.locator("#messages");
    await rootNode.evaluate((root) => {
      root.scrollTop = 20 * 72 - 16;
      root.dispatchEvent(new Event("scroll"));
    });
    await expect(anchor).toBeAttached();
    await page.waitForFunction(() => {
      const root = document.querySelector("#messages");
      const target = document.querySelector('[data-message="message-20"]');
      if (!target) return false;
      const bounds = root.getBoundingClientRect();
      const box = target.getBoundingClientRect();
      return box.top >= bounds.top && box.top < bounds.bottom;
    });
    await rootNode.evaluate((root) => root.dispatchEvent(new Event("scroll")));
    const originalOffset = await anchor.evaluate(
      (node) =>
        node.getBoundingClientRect().top -
        document.querySelector("#messages").getBoundingClientRect().top,
    );
    expect(originalOffset).toBeCloseTo(16, 0);
    await page.evaluate(() => window.appendForwardPage());
    await page.evaluate(async () => {
      for (let frame = 0; frame < 25; frame++)
        await new Promise((resolve) => requestAnimationFrame(resolve));
      window.releaseScroll();
    });
    const restored = await page.evaluate(() => {
      const root = document.querySelector("#messages");
      const message = document.querySelector('[data-message="message-20"]');
      return message
        ? {
            connected: !!message.closest("[data-history-row]"),
            offset:
              message.getBoundingClientRect().top -
              root.getBoundingClientRect().top,
          }
        : { connected: false, offset: Number.NaN };
    });
    expect(
      restored.connected,
      "the anchor row remains in the rendered window",
    ).toBe(true);
    expect(
      Math.abs(restored.offset - originalOffset),
      "the anchor returns to its viewport offset",
    ).toBeLessThan(2);
  } finally {
    await context.close();
    await server.close();
    await rm(cacheDir, { recursive: true, force: true });
  }
});
