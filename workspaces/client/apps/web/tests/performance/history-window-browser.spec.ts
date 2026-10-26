import { fixtureOrigin } from "./performance-fixture.ts";
import type {} from "./history-window.fixture.tsx";
import { mkdtemp, rm, readFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { test, expect } from "../playwright.ts";

test("long history mounts a bounded viewport and preserves navigation and anchors", async ({
  browser,
}) => {
  test.setTimeout(90000);
  const root = fileURLToPath(new URL("../..//", import.meta.url));
  const { createServer } = await import("vite");
  const cacheDir = await mkdtemp(join(tmpdir(), "studio-history-window-vite-"));
  const entry = new URL("./history-window.fixture.tsx", import.meta.url)
    .pathname;
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
        "dexie",
      ],
    },
    server: { host: "127.0.0.1", port: 0 },
    plugins: [
      {
        name: "history-window-audit",
        enforce: "pre",
        async transform(_code, id) {
          if (
            process.env.STUDIO_HISTORY_BASELINE &&
            id.endsWith("/transcript/HistoryWindow.tsx")
          )
            return {
              code: await readFile(process.env.STUDIO_HISTORY_BASELINE, "utf8"),
              map: null,
            };
        },
        configureServer(s) {
          s.middlewares.use("/audit", (_q, r) => {
            r.setHeader("Content-Type", "text/html");
            r.end(
              '<div id="root"></div><script type="module" src="/history-window-audit.tsx"></script>',
            );
          });
        },
        resolveId(id) {
          if (id === "/history-window-audit.tsx") return entry;
        },
      },
    ],
  });
  const context = await browser.newContext({
    viewport: { width: 1100, height: 900 },
  });
  const page = await context.newPage();
  const errors: string[] = [];
  page.on("pageerror", (e) => errors.push(e.message));
  await page.route("**/api/**", (route) => route.fulfill({ json: {} }));
  const mounted = () => page.locator("[data-history-row]").count();
  const offset = (id: string) =>
    page
      .locator(`[data-message="${id}"]`)
      .evaluate(
        (node) =>
          node.getBoundingClientRect().top -
          (document.querySelector("#messages")?.getBoundingClientRect().top ??
            NaN),
      );
  try {
    await server.listen();
    const url = `${fixtureOrigin(server)}/audit`;
    await page.goto(url);
    await expect(page.locator('[data-message="a999"]')).toBeVisible();
    expect(await page.locator("[data-message]").count()).toBeLessThan(150);
    await expect(page.locator("[data-history-window]")).toBeVisible();
    expect(await mounted()).toBeLessThanOrEqual(24);
    expect(
      await page.evaluate(() => window.historyWindowRenderedMessages),
    ).toBeLessThan(150);
    await page.locator("#message").fill("Preserve this draft");
    await page.evaluate(() => window.historyWindowJump("a500"));
    await expect(page.locator('[data-message="a500"]')).toBeVisible();
    await expect(page.getByAltText("Saved attachment")).toBeVisible();
    await expect
      .poll(async () => Math.abs((await offset("a500")) - 16))
      .toBeLessThan(2);
    const original = await offset("a500");
    await page.evaluate(() => window.historyWindowPrepend());
    await expect
      .poll(async () => Math.abs((await offset("a500")) - original))
      .toBeLessThan(2);
    await page
      .locator("#messages")
      .evaluate((node) => (node.style.width = "510px"));
    await expect
      .poll(async () => Math.abs((await offset("a500")) - original))
      .toBeLessThan(2);
    await page
      .locator("#messages")
      .evaluate((node) => (node.style.width = "720px"));
    await page.evaluate(() => window.historyWindowJump("original-tool500-1"));
    const tool = page.locator(
      '[data-message="tool500-1"]:not([data-lazy-message])',
    );
    await expect(tool).toBeVisible();
    await tool.evaluate((node) => {
      node.addEventListener(
        "toggle",
        () => {
          node.dataset.toggleObserved = "true";
        },
        { once: true },
      );
    });
    await tool.locator("summary").first().click();
    await expect(tool).toHaveAttribute("open", "");
    await expect(tool).toHaveAttribute("data-toggle-observed", "true");
    await page.evaluate(() => window.historyWindowJump("a900"));
    await expect
      .poll(() => page.locator('[data-message="tool500-1"]').count())
      .toBe(0);
    await page.evaluate(() => window.historyWindowJump("tool500-1"));
    await expect(
      page.locator('[data-message="tool500-1"]:not([data-lazy-message])'),
    ).toHaveAttribute("open", "");
    await page
      .getByRole("button", { name: "Preview attachment", exact: true })
      .click();
    await expect(page.getByRole("dialog")).toBeVisible();
    await page.evaluate(() => window.historyWindowJump("a900"));
    await expect(page.getByRole("dialog")).toBeVisible();
    await expect(page.locator('[data-message="a500"]')).toBeAttached();
    await page
      .getByRole("button", { name: "Close preview", exact: true })
      .click();
    await page.evaluate(() => window.historyWindowJump("a500"));
    // A native selection survives eviction of its old viewport.
    await page
      .locator('[data-message="a500"] p')
      .first()
      .evaluate((node) => {
        const range = document.createRange();
        range.selectNodeContents(node);
        const selection = document.getSelection();
        selection?.removeAllRanges();
        selection?.addRange(range);
      });
    const selection = await page.evaluate(() =>
      document.getSelection()?.toString(),
    );
    await page.evaluate(() => window.historyWindowJump("a900"));
    await expect
      .poll(() => page.evaluate(() => document.getSelection()?.toString()))
      .toBe(selection);
    expect(await mounted()).toBeLessThanOrEqual(50);
    await page.evaluate(() => document.getSelection()?.removeAllRanges());
    await page.evaluate(() => window.historyWindowJump("a500"));
    await expect(page.locator("#message")).toHaveValue("Preserve this draft");
    const savedOffset = await offset("a500");
    // HistoryWindow publishes its visible range, then persists the anchor.
    await page.evaluate(
      () =>
        new Promise((resolve) =>
          requestAnimationFrame(() => requestAnimationFrame(resolve)),
        ),
    );
    await page.reload();
    await expect(page.locator('[data-message="a500"]')).toBeVisible();
    await expect
      .poll(async () => Math.abs((await offset("a500")) - savedOffset))
      .toBeLessThan(2);
    await page.locator("#message").fill("Send after history navigation");
    await page.getByRole("button", { name: "Send", exact: true }).click();
    await expect(page.locator("output")).toHaveText(
      "Send after history navigation",
    );
    await page.evaluate(() => {
      window.historyWindowLatest();
      window.historyWindowAppendStream();
    });
    await expect(page.locator('[data-message="stream"]')).toBeVisible();
    expect(await mounted()).toBeLessThanOrEqual(24);
    const latestRows = await mounted();
    const latestNodes = await page.locator("[data-message]").count();
    await page.evaluate(() => window.historyWindowLargeTurn());
    await expect(page.locator('[data-message="large-2999"]')).toBeAttached();
    expect(await page.locator("[data-message]").count()).toBeLessThanOrEqual(
      512,
    );
    await page.evaluate(() => window.historyWindowJump("large-20"));
    await expect(
      page.locator('[data-message="large-20"]:not([data-lazy-message])'),
    ).toBeVisible();
    expect(await page.locator(".tool-card").count()).toBeLessThanOrEqual(512);
    const sameTurnTarget = page.locator(
      '[data-message="large-20"]:not([data-lazy-message])',
    );
    await sameTurnTarget.locator("summary").first().click();
    await sameTurnTarget.locator("summary").first().focus();
    await sameTurnTarget
      .locator("summary")
      .first()
      .evaluate((node) => {
        const range = document.createRange();
        range.selectNodeContents(node);
        document.getSelection()?.removeAllRanges();
        document.getSelection()?.addRange(range);
        window.historyWindowSameTurnNode = node;
      });
    const sameTurnSelection = await page.evaluate(() =>
      document.getSelection()?.toString(),
    );
    const sameTurnOffset = await sameTurnTarget.evaluate(
      (node) =>
        node.getBoundingClientRect().top -
        (document.querySelector("#messages")?.getBoundingClientRect().top ??
          NaN),
    );
    await page.evaluate(() => window.historyWindowPrependSameTurn());
    await expect
      .poll(() =>
        page.evaluate(
          () =>
            window.historyWindowSameTurnNode.isConnected &&
            document.activeElement === window.historyWindowSameTurnNode,
        ),
      )
      .toBe(true);
    await expect
      .poll(() => page.evaluate(() => document.getSelection()?.toString()))
      .toBe(sameTurnSelection);
    await expect
      .poll(async () =>
        Math.abs(
          (await sameTurnTarget.evaluate(
            (node) =>
              node.getBoundingClientRect().top -
              (document.querySelector("#messages")?.getBoundingClientRect()
                .top ?? NaN),
          )) - sameTurnOffset,
        ),
      )
      .toBeLessThan(2);
    await page.evaluate(() => document.getSelection()?.removeAllRanges());
    await page.evaluate(() => window.historyWindowJump("large-20"));
    const sameTurnReloadOffset = await sameTurnTarget.evaluate(
      (node) =>
        node.getBoundingClientRect().top -
        (document.querySelector("#messages")?.getBoundingClientRect().top ??
          NaN),
    );
    await expect
      .poll(() =>
        page.evaluate(() => {
          const anchor: unknown = JSON.parse(
            localStorage.getItem("history-window-browser:window-anchor") ||
              "null",
          );
          return (
            anchor !== null &&
            typeof anchor === "object" &&
            "kind" in anchor &&
            anchor.kind === "message" &&
            "id" in anchor &&
            typeof anchor.id === "string" &&
            anchor.id.startsWith("large-")
          );
        }),
      )
      .toBe(true);
    await page.reload();
    await expect(sameTurnTarget).toBeVisible();
    await expect
      .poll(async () =>
        Math.abs(
          (await sameTurnTarget.evaluate(
            (node) =>
              node.getBoundingClientRect().top -
              (document.querySelector("#messages")?.getBoundingClientRect()
                .top ?? NaN),
          )) - sameTurnReloadOffset,
        ),
      )
      .toBeLessThan(2);
    await page.evaluate(() => window.historyWindowSmall());
    await expect(page.locator('[data-message="a0"]')).toBeAttached();
    await expect(page.locator("[data-history-window]")).toHaveCount(0);
    const smallTool = page.locator(
      '[data-message="tool0-0"]:not([data-lazy-message])',
    );
    await smallTool.locator("summary").first().click();
    await expect(smallTool).toHaveAttribute("open", "");
    await page.locator(".turn-work > summary").click();
    await expect(page.locator(".tool-card")).toHaveCount(0);
    await page.locator(".turn-work > summary").click();
    await expect(smallTool).toHaveAttribute("open", "");
    await page.evaluate(() => window.historyWindowJump("original-tool0-1"));
    await expect(
      page.locator('[data-message="tool0-1"]:not([data-lazy-message])'),
    ).toBeVisible();
    for (const many of [false, true]) {
      await page.evaluate((many) => {
        window.historyWindowLegacy(many);
        window.historyWindowLatest();
      }, many);
      await expect(
        page.locator(`[data-message="a${many ? 999 : 0}"]`),
      ).toBeVisible();
      await page.evaluate(() => window.historyWindowJump("original-tool0-1"));
      await expect(
        page.locator('[data-message="tool0-1"]:not([data-lazy-message])'),
      ).toBeVisible();
    }
    for (const many of [false, true]) {
      await page.evaluate((many) => {
        window.historyWindowOptimistic(many);
        window.historyWindowLatest();
      }, many);
      const optimistic = page.locator('[data-message="optimistic:user"]');
      await expect(optimistic).toBeVisible();
      await optimistic.evaluate(
        (node) => (window.historyWindowOptimisticNode = node),
      );
      await optimistic
        .getByRole("button", { name: "Preview attachment", exact: true })
        .click();
      await expect(page.getByRole("dialog")).toBeVisible();
      await page.evaluate(() => window.historyWindowAck());
      await expect(page.getByRole("dialog")).toBeVisible();
      expect(
        await page.evaluate(
          () =>
            window.historyWindowOptimisticNode ===
            document.querySelector('[data-message="durable-user"]'),
        ),
      ).toBe(true);
      await page
        .getByRole("button", { name: "Close preview", exact: true })
        .click();
    }
    expect(errors).toEqual([]);
    console.log(
      JSON.stringify({
        loadedMessages: 5000,
        mountedRows: latestRows,
        mountedMessageNodes: latestNodes,
        renderedMessages: await page.evaluate(
          () => window.historyWindowRenderedMessages,
        ),
      }),
    );
  } finally {
    await context.close();
    await server.close();
    await rm(cacheDir, { recursive: true, force: true });
  }
});
