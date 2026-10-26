import {
  readTestState,
  test,
  spawnFixture as spawn,
  waitForFixturePort,
  rustUiFixture,
} from "../playwright.ts";
import { required } from "./helpers.ts";
import assert from "node:assert/strict";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

// Messages sent while the server does not answer stay in the local outbox
// ("Sending…", "Waiting to send"). Their delivery row needs its own space: it
// must not cover the next transcript item or the message action buttons.
const layouts = [
  {
    name: "touch 951x669",
    viewport: { width: 951, height: 669 },
    touch: true,
  },
  {
    name: "desktop 1440x960",
    viewport: { width: 1440, height: 960 },
    touch: false,
  },
];

for (const layout of layouts)
  test(`outgoing message delivery rows do not overlap at ${layout.name}`, async ({
    browser,
  }) => {
    test.setTimeout(60_000);
    const evidence = await mkdtemp(join(tmpdir(), "studio-delivery-row-"));
    const fixture = spawn(process.execPath, [rustUiFixture, evidence], {
      stdio: ["ignore", "pipe", "pipe"],
      env: { ...process.env, FIXTURE_WORKER_COUNT: "1" },
    });
    let log = "";
    required(fixture.stderr).on("data", (chunk) => (log += chunk));
    const port = await waitForFixturePort(fixture, {
      label: "Delivery row geometry",
      diagnostics: () => log,
    });
    const origin = `http://127.0.0.1:${port}`;
    await readTestState(origin);
    const context = await browser.newContext({
      viewport: layout.viewport,
      hasTouch: layout.touch,
      isMobile: layout.touch,
    });
    const page = await context.newPage();
    page.setDefaultTimeout(10_000);
    // The send request never completes, as on a lost connection.
    await page.route("**/api/messages", () => {});
    await page.goto(origin);
    await page.locator(".chat-row").filter({ hasText: "Release lead" }).click();
    const texts = ["First outgoing message", "Second outgoing message"];
    for (const text of texts) {
      await page.locator("#message").fill(text);
      await page.locator("#send").click();
      await page
        .locator("#messages article.message.user")
        .filter({ hasText: text })
        .locator(".message-delivery-inline, .message-delivery-heading")
        .waitFor({ state: "visible" });
    }
    await page.screenshot({
      path: test
        .info()
        .outputPath(`delivery-rows-${layout.viewport.width}.png`),
    });
    const overlaps = await page.evaluate(() => {
      const visible = (element: Element) => {
        const rect = element.getBoundingClientRect();
        return (
          rect.width > 0 &&
          rect.height > 0 &&
          getComputedStyle(element).visibility !== "hidden"
        );
      };
      const intersect = (a: DOMRect, b: DOMRect) =>
        a.left < b.right - 0.5 &&
        b.left < a.right - 0.5 &&
        a.top < b.bottom - 0.5 &&
        b.top < a.bottom - 0.5;
      const name = (element: Element) =>
        element.getAttribute("aria-label") ||
        element.textContent?.trim().slice(0, 30) ||
        element.className;
      const found: string[] = [];
      for (const message of document.querySelectorAll(
        "#messages article.message.user",
      )) {
        const rows = [
          ...message.querySelectorAll(
            ".message-delivery-inline, .message-delivery-heading",
          ),
        ];
        const rowParts = rows
          .flatMap((row) => [row, ...row.querySelectorAll("button")])
          .filter(visible);
        const actions = [
          ...message.querySelectorAll(".message-bottom button"),
        ].filter(visible);
        const others = [
          ...document.querySelectorAll("#messages article.message"),
          ...document.querySelectorAll("#messages > *"),
        ].filter(
          (other) =>
            other !== message && !other.contains(message) && visible(other),
        );
        const pairs = [
          ...rowParts.flatMap((part) =>
            [...actions, ...others].map((other) => [part, other] as const),
          ),
          ...(rows.length ? actions : []).flatMap((part) =>
            others.map((other) => [part, other] as const),
          ),
        ];
        for (const [part, other] of pairs)
          if (
            intersect(
              part.getBoundingClientRect(),
              other.getBoundingClientRect(),
            )
          )
            found.push(`${name(part)} covers ${name(other)}`);
      }
      return found;
    });
    assert.deepEqual(overlaps, []);
    await context.close();
  });
