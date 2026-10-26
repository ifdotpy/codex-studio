import { writeFileSync } from "node:fs";
import { test, expect, entityPullFixtureForRequest } from "../playwright.ts";
import { sidebarParityFixture } from "../sidebar/sidebar-parity-fixture.ts";
import { fixtureWorkspaceId } from "../servers/multi-server-fixture.ts";

const sizes = [
  {
    name: "desktop",
    width: 1440,
    height: 900,
    touch: false,
    right: 0,
    bottom: 0,
    duo: false,
  },
  {
    name: "tablet",
    width: 1024,
    height: 768,
    touch: true,
    right: 0,
    bottom: 20,
    duo: false,
  },
  {
    name: "phone-portrait",
    width: 390,
    height: 844,
    touch: true,
    right: 0,
    bottom: 34,
    duo: false,
  },
  {
    name: "phone-landscape",
    width: 844,
    height: 390,
    touch: true,
    right: 59,
    bottom: 21,
    duo: false,
  },
  {
    name: "duo-shell",
    width: 951,
    height: 669,
    touch: true,
    right: 84,
    bottom: 34,
    duo: true,
  },
  {
    name: "duo-pwa",
    width: 867,
    height: 669,
    touch: true,
    right: 0,
    bottom: 34,
    duo: true,
  },
];

for (const size of sizes) {
  test.describe(size.name, () => {
    test.use({
      hasTouch: size.touch,
      reducedMotion: "reduce",
      colorScheme: "dark",
      serviceWorkers: "block",
    });
    test("keeps the Duo proportions within their viewport rule", async ({
      page,
      context,
    }) => {
      const fixture = await sidebarParityFixture(context, { paired: false });
      fixture.local.snapshot.runtime.requests = [];
      fixture.local.snapshot.runtime.complaints = [];
      let toolExample = false;
      // Match runtime-http's script policy. The older multi-server fixture
      // omits the WebAssembly permission required by the transcript parser.
      await page.route(
        (url) => url.origin === fixture.local.origin && url.pathname === "/",
        async (route) => {
          const response = await route.fetch();
          const headers = response.headers();
          headers["content-security-policy"] = headers[
            "content-security-policy"
          ].replace(
            "script-src 'self'",
            "script-src 'self' 'wasm-unsafe-eval'",
          );
          await route.fulfill({ response, headers });
        },
      );
      await page.route("**/api/sync/pull?*", (route) => {
        const url = route.request().url();
        if (!new URL(url).searchParams.get("scope")?.startsWith("transcript:"))
          return route.fallback();
        return route.fulfill({
          json: {
            workspaceId: fixtureWorkspaceId("Local"),
            ...entityPullFixtureForRequest(fixture.local.snapshot, url, {
              transcript: {
                agent: { id: "overlap", status: "completed", inFlight: false },
                items: toolExample
                  ? [
                      {
                        id: "tool-ask",
                        role: "user",
                        text: "Check the layout.",
                        turnId: "tools",
                        turnStatus: "completed",
                      },
                      {
                        id: "tool-command",
                        role: "output",
                        text: JSON.stringify({
                          type: "mcpToolCall",
                          name: "inspect_layout",
                          status: "completed",
                          exitCode: 0,
                          result: {
                            content: [{ type: "text", text: "Layout checked" }],
                          },
                        }),
                        turnId: "tools",
                        turnStatus: "completed",
                        toolStatus: "completed",
                      },
                      {
                        id: "tool-patch",
                        role: "output",
                        text: JSON.stringify({
                          type: "fileChange",
                          status: "completed",
                          changes: [
                            {
                              path: "docs/layout.md",
                              diff: "@@ -1 +1 @@\n-Old proportions\n+Duo proportions",
                            },
                          ],
                        }),
                        turnId: "tools",
                        turnStatus: "completed",
                        toolStatus: "completed",
                      },
                      {
                        id: "tool-answer",
                        role: "assistant",
                        text: "Layout checks passed.\n\n[Layout report](</workspace/layout-report.md>)",
                        turnId: "tools",
                        turnStatus: "completed",
                        phase: "final_answer",
                      },
                    ]
                  : [
                      {
                        id: "proportions-message",
                        role: "assistant",
                        text:
                          "The conversation keeps a clear reading column. The sidebar leaves enough space for the title and each conversation. The composer stays above the Home area.\n\n" +
                          "This paragraph gives the layout enough text to measure line spacing and column width. ".repeat(
                            5,
                          ),
                      },
                    ],
                truncated: false,
                nextCursor: null,
                nextAfterCursor: null,
                historyVersion: "duo-proportions",
              },
            }),
          },
        });
      });
      const cdp = await context.newCDPSession(page);
      await page.setViewportSize({ width: 1200, height: size.height });
      try {
        const sidebar = await fixture.open(page);
        await sidebar
          .getByRole("button", { name: /^Local Saved pin\b/ })
          .first()
          .click();
        await cdp.send("Emulation.setSafeAreaInsetsOverride", {
          insets: { top: 0, right: size.right, bottom: size.bottom, left: 0 },
        });
        await page.setViewportSize({ width: size.width, height: size.height });
        await expect(page.locator("#messages .prose")).toContainText(
          "The conversation keeps a clear reading column.",
        );
        await expect(page.locator("#composer")).toBeVisible();
        await page.evaluate(() => document.fonts.ready);
        const measurements = await page.evaluate(() => {
          const selectors = [
            "#sidebar",
            ".sidebar-header",
            "#chat-list",
            ".chat-row",
            ".row-copy strong",
            ".workspace-header",
            "#conversation",
            "#messages",
            "#messages .message-content",
            "#messages .message.assistant",
            "#messages .prose",
            "#composer",
            "#message",
            ".conversation-composer-footer",
            ".edge-rail",
            ".edge-rail button",
          ];
          return Object.fromEntries(
            selectors.map((selector) => {
              const node = [
                ...document.querySelectorAll<HTMLElement>(selector),
              ].find((item) => item.getClientRects().length);
              if (!node) return [selector, null];
              const r = node.getBoundingClientRect();
              const s = getComputedStyle(node);
              return [
                selector,
                {
                  x: r.x,
                  y: r.y,
                  width: r.width,
                  height: r.height,
                  bottom: r.bottom,
                  fontSize: parseFloat(s.fontSize),
                  lineHeight: parseFloat(s.lineHeight),
                  radius: parseFloat(s.borderRadius),
                  gap: parseFloat(s.rowGap),
                  computedWidth: s.width,
                  maxWidth: s.maxWidth,
                  boxSizing: s.boxSizing,
                  readingMax: s.getPropertyValue("--layout-duo-reading-max"),
                  bodyFont: s.getPropertyValue("--layout-duo-body-font"),
                  sidebarFont: s.getPropertyValue("--studio-sidebar-font-size"),
                  mainFont: s.getPropertyValue("--studio-main-font-size"),
                  inlineStyle: node.style.cssText,
                  borderWidth: parseFloat(s.borderLeftWidth),
                  paddingInline: parseFloat(s.paddingLeft),
                  mask: s.maskImage,
                  background: s.backgroundColor,
                },
              ];
            }),
          );
        });
        writeFileSync(
          test.info().outputPath("measurements.json"),
          JSON.stringify({ size, measurements }, null, 2),
        );
        const boxes = await page.evaluate(() =>
          [...document.body.querySelectorAll("*")].flatMap((node) => {
            const r = node.getBoundingClientRect();
            if (!r.width || !r.height) return [];
            const path: string[] = [];
            for (
              let parent: Element | null = node;
              parent && parent !== document.body;
              parent = parent.parentElement
            ) {
              const siblings = parent.parentElement?.children;
              path.unshift(
                `${parent.tagName}:${siblings ? [...siblings].indexOf(parent) : 0}`,
              );
            }
            return [
              {
                path: path.join("/"),
                x: r.x,
                y: r.y,
                width: r.width,
                height: r.height,
              },
            ];
          }),
        );
        writeFileSync(
          test.info().outputPath("layout-boxes.json"),
          JSON.stringify({ size, boxes }, null, 2),
        );
        await page.screenshot({
          path: test.info().outputPath(`${size.name}.png`),
        });
        if (size.duo) {
          await expect(
            page.locator(".viewport-bottom-band .usage-passive-band"),
          ).toHaveCount(0);
          await expect(
            page.locator(".usage-footer .session-cost-summary"),
          ).toHaveCount(1);
          expect(measurements["#sidebar"]?.width).toBeCloseTo(316.8, 1);
          expect(measurements[".chat-row"]?.height).toBeGreaterThanOrEqual(50);
          expect(measurements[".row-copy strong"]?.fontSize).toBe(17);
          expect(measurements["#messages .message-content"]?.width).toBeCloseTo(
            504,
            0,
          );
          expect(
            measurements["#messages .message.assistant"]?.borderWidth,
          ).toBe(1);
          expect(
            measurements["#messages .message.assistant"]?.background,
          ).not.toBe("rgba(0, 0, 0, 0)");
          expect(measurements["#messages .prose"]?.width).toBeCloseTo(478, 0);
          expect(
            measurements["#messages .message.assistant"]?.paddingInline,
          ).toBeGreaterThanOrEqual(12);
          expect(measurements["#messages"]?.mask).toContain("linear-gradient");
          expect(measurements["#messages .prose"]?.lineHeight).toBeCloseTo(
            27.2,
            1,
          );
          expect(measurements["#composer"]?.height).toBeGreaterThanOrEqual(97);
          expect(measurements["#composer"]?.radius).toBe(24);
          expect(measurements["#composer"]?.width).toBe(504);
          expect(measurements["#composer"]?.x).toBe(
            measurements["#messages .message.assistant"]?.x,
          );
          expect(measurements["#composer"]?.width).toBe(
            measurements["#messages .message.assistant"]?.width,
          );
          expect(measurements["#composer"]?.bottom).toBeCloseTo(
            size.height - size.bottom,
            0,
          );
          expect(
            measurements[".conversation-composer-footer"]?.bottom,
          ).toBeLessThanOrEqual(measurements["#composer"]!.y);
          if (size.right) {
            expect(measurements[".edge-rail button"]?.y).toBe(124);
            expect(measurements[".edge-rail"]?.gap).toBe(13);
          }
          await page.evaluate(() => {
            document.documentElement.style.setProperty(
              "--studio-main-font-size",
              "20px",
            );
            document.documentElement.style.setProperty(
              "--studio-sidebar-font-size",
              "20px",
            );
          });
          await expect(page.locator("#messages .prose")).toHaveCSS(
            "font-size",
            "20px",
          );
          await expect(page.locator(".row-copy strong").first()).toHaveCSS(
            "font-size",
            "20px",
          );
          if (size.right) {
            toolExample = true;
            await page.reload();
            await expect(
              page.locator(".conversation-result").first(),
            ).toBeVisible();
            await expect(page.locator("#messages [role=alert]")).toHaveCount(0);
            for (const group of await page
              .locator("details.turn-work:not([open])")
              .all())
              await group.locator(":scope > summary").click();
            await expect(page.locator(".turn-work").first()).toBeVisible();
            await expect(page.locator(".tool-card").first()).toBeVisible();
            await expect(
              page.locator(".file-change-card").first(),
            ).toBeVisible();
            await expect(
              page.locator(".message.assistant .message-bottom"),
            ).toBeVisible();
            await expect(page.locator(".message.user")).not.toHaveCSS(
              "background-color",
              "rgba(0, 0, 0, 0)",
            );
            await page.screenshot({
              path: test.info().outputPath("duo-tools-results.png"),
            });
          }
        } else
          await expect(page.locator("html")).not.toHaveAttribute(
            "data-duo-proportions",
          );
      } finally {
        await fixture.close();
      }
    });
  });
}
