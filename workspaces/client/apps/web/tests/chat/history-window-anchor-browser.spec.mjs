import { createRequire } from "node:module";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { expect, test } from "../playwright.mjs";

async function createHarness(browser, start, count) {
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
  const [rows, setRows] = useState(() => makeRows(${start}, ${count}));
  const scroll = useRef(null);
  window.scrollToTopAsUser = () => {
    const root = scroll.current;
    root.dispatchEvent(new WheelEvent("wheel", {deltaY: -1}));
    const holdScroll = event => event.stopImmediatePropagation();
    root.addEventListener("scroll", holdScroll, true);
    root.scrollTop = 0;
    root.dispatchEvent(new Event("scroll"));
    root.removeEventListener("scroll", holdScroll, true);
  };
  window.scrollToMessage = (index) => {
    const root = scroll.current;
    root.scrollTop = index * 72 - 16;
    root.dispatchEvent(new Event("scroll"));
  };
  window.commitViewportAnchor = () => scroll.current.dispatchEvent(new Event("scroll"));
  window.prependPage = (start) => flushSync(() => setRows(current => [...makeRows(start, 120), ...current]));
  window.appendForwardPage = () => {
    const root = scroll.current;
    const holdScroll = event => event.stopImmediatePropagation();
    root.addEventListener("scroll", holdScroll, true);
    flushSync(() => setRows(current => [...current, ...makeRows(160, 60)]));
    root.scrollTop = root.scrollHeight;
    window.releaseScroll = () => root.removeEventListener("scroll", holdScroll, true);
  };
  window.jumpToOldest = () => {
    const root = scroll.current;
    root.dispatchEvent(new Event("studio-history-reset-anchor"));
    const holdScroll = event => event.stopImmediatePropagation();
    root.addEventListener("scroll", holdScroll, true);
    root.scrollTop = 0;
    root.dispatchEvent(new Event("scroll"));
    root.removeEventListener("scroll", holdScroll, true);
    flushSync(() => setRows(current => [...makeRows(0, 120), ...current]));
  };
  return <div id="messages" ref={scroll} style={{height: 500, overflow: "auto", overflowAnchor: "none", position: "relative"}}>
    <HistoryWindow rows={rows} scrollContainer={scroll} hasRecentScrollInput={() => performance.now() - (window.__lastInput ?? -Infinity) < 600} storageKey="history-window-anchor-audit" render={row => <article data-message={row.items[0].id} style={{height: 72, minHeight: 72, boxSizing: "border-box"}}>{row.items[0].text}</article>} />
  </div>;
}
const root = document.querySelector("#root");
createRoot(root).render(<Harness/>);
document.querySelector("#messages").addEventListener("wheel", () => { window.__lastInput = performance.now(); });`;
        },
      },
    ],
  });
  const context = await browser.newContext({
    viewport: { width: 900, height: 700 },
  });
  const page = await context.newPage();
  await server.listen();
  await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/audit`);
  await page.locator(`[data-message="message-${start}"]`).waitFor();
  return {
    page,
    async close() {
      await context.close();
      await server.close();
      await rm(cacheDir, { recursive: true, force: true });
    },
  };
}

async function waitForFrames(page, count = 4) {
  await page.evaluate(
    (frames) =>
      new Promise((resolve) => {
        const next = () =>
          frames-- > 0 ? requestAnimationFrame(next) : resolve();
        requestAnimationFrame(next);
      }),
    count,
  );
}

async function expectMessageInViewport(page, id) {
  await page.waitForFunction(
    (messageId) => {
      const root = document.querySelector("#messages");
      const message = Array.from(root.querySelectorAll("[data-message]")).find(
        (candidate) => candidate.dataset.message === messageId,
      );
      if (!message) return false;
      const bounds = root.getBoundingClientRect();
      const box = message.getBoundingClientRect();
      return (
        box.height > 0 && box.bottom > bounds.top && box.top < bounds.bottom
      );
    },
    id,
    { timeout: 5_000 },
  );
}

test("forward paging keeps the reading row mounted and at its viewport offset", async ({
  browser,
}) => {
  test.setTimeout(90_000);
  const { page, close } = await createHarness(browser, 0, 160);
  try {
    const anchor = page.locator('[data-message="message-20"]');
    await page.evaluate(() => window.scrollToMessage(20));
    await expect(anchor).toBeAttached();
    await waitForFrames(page);
    await page.evaluate(() => window.commitViewportAnchor());
    await waitForFrames(page, 2);
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
    await close();
  }
});

test("upward scrolling through prepends reaches the oldest row", async ({
  browser,
}) => {
  const { page, close } = await createHarness(browser, 240, 120);
  try {
    await page.evaluate(() => window.scrollToMessage(10));
    await waitForFrames(page);
    await page.evaluate(() => window.commitViewportAnchor());
    await waitForFrames(page, 2);
    await page.evaluate(() => window.scrollToTopAsUser());
    await page.evaluate(() => window.prependPage(120));
    await waitForFrames(page);
    await page.evaluate(() => window.scrollToTopAsUser());
    await page.evaluate(() => window.prependPage(0));
    await expectMessageInViewport(page, "message-0");
  } finally {
    await close();
  }
});

test("an explicit jump to the oldest row is not undone by a prepend", async ({
  browser,
}) => {
  const { page, close } = await createHarness(browser, 120, 120);
  try {
    await page.evaluate(() => window.scrollToMessage(20));
    await waitForFrames(page);
    await page.evaluate(() => window.commitViewportAnchor());
    await waitForFrames(page, 2);
    await page.evaluate(() => window.jumpToOldest());
    await waitForFrames(page, 25);
    await expectMessageInViewport(page, "message-0");
  } finally {
    await close();
  }
});
