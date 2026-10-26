import {
  test,
  expect,
  spawnFixture,
  rustUiFixture,
  stubSessionCost,
} from "../playwright.ts";
import { mkdtemp, mkdir, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

test("offline send has one saved-message status and preserves its delivery identity", async ({
  browser,
}) => {
  test.setTimeout(120_000);
  const state = await mkdtemp(join(tmpdir(), "studio-offline-feedback-"));
  const evidence = process.env.OFFLINE_SEND_EVIDENCE || join(state, "evidence");
  await mkdir(evidence, { recursive: true });
  const fixture = spawnFixture(process.execPath, [rustUiFixture, state], {
    stdio: ["ignore", "pipe", "pipe"],
  });
  let log = "";
  fixture.stderr.on("data", (chunk) => (log += chunk));
  try {
    const port = await new Promise((resolve, reject) => {
      fixture.stdout.once("data", (chunk) =>
        resolve(Number(String(chunk).trim())),
      );
      fixture.once("exit", () => reject(Error(log)));
    });
    for (const width of [390, 320, 1440])
      for (const scheme of ["light", "dark"]) {
        const page = await browser.newPage({
          viewport: { width, height: width === 320 ? 568 : 844 },
          isMobile: width < 760,
          hasTouch: width < 760,
          colorScheme: scheme,
        });
        const text = `Saved offline ${width} ${scheme}`;
        const posts = [];
        let release;
        const deliveryGate = new Promise((resolve) => (release = resolve));
        try {
          await stubSessionCost(page);
          await page.route("**/api/messages", async (route) => {
            if (route.request().method() !== "POST") return route.fallback();
            const body = route.request().postDataJSON();
            if (body.text !== text) return route.fallback();
            posts.push(body);
            await deliveryGate;
            await route.continue();
          });
          await page.goto(`http://127.0.0.1:${port}`);
          await page.getByLabel("Toggle conversations").waitFor();
          if (!(await page.locator("#sidebar").isVisible()))
            await page.getByLabel("Toggle conversations").click();
          await page
            .locator("[data-chat]")
            .filter({ hasText: "Release lead" })
            .first()
            .click();
          await expect(page.locator("#messages")).toContainText(
            "Review the release",
          );
          await expect(page.locator(".session-cost-summary")).toHaveAttribute(
            "aria-busy",
            "false",
          );
          await page.evaluate(
            (s) =>
              document.documentElement.setAttribute(
                "data-mantine-color-scheme",
                s,
              ),
            scheme,
          );
          await page.context().setOffline(true);
          await page.locator("#message").fill(text);
          await page
            .getByRole("button", { name: "Send message", exact: true })
            .click();
          const status = page.getByRole("status").filter({
            hasText:
              "Connection lost. Your message is saved and will send when connected.",
          });
          await expect(status).toHaveCount(1);
          await expect(status).toBeVisible();
          await expect(
            page.getByRole("button", { name: "Edit message", exact: true }),
          ).toBeVisible();
          await expect(
            page.getByRole("button", { name: "Cancel send", exact: true }),
          ).toBeVisible();
          expect(await page.locator("body").innerText()).not.toContain(
            "Failed to fetch",
          );
          const details = status.locator("details");
          await expect(details.locator("pre").first()).toContainText(
            /Failed to fetch|The device is offline/,
          );
          await expect(details.locator("pre").first()).toBeHidden();
          await details.locator("summary").click();
          await expect(details.locator("pre").first()).toBeVisible();
          await details.locator("summary").click();
          await page.screenshot({
            path: join(evidence, `offline-${width}-${scheme}.png`),
          });
          await page.context().setOffline(false);
          await expect.poll(() => posts.length).toBe(1);
          // A network event alone cannot clear the status before acceptance.
          await expect(status).toBeVisible();
          release();
          await expect(status).toHaveCount(0);
          await expect(page.locator(".outbox-controls")).toHaveCount(0);
          await expect(page.locator("#messages")).toContainText(text);
          expect(new Set(posts.map((p) => p.id)).size).toBe(1);
          expect(posts).toHaveLength(1);
          expect(await page.locator("body").innerText()).not.toContain(
            "Failed to fetch",
          );
          await page.screenshot({
            path: join(evidence, `recovered-${width}-${scheme}.png`),
          });
        } catch (error) {
          await page.screenshot({
            path: join(evidence, `failed-${width}-${scheme}.png`),
          });
          await writeFile(
            join(evidence, `failed-${width}-${scheme}.txt`),
            await page.locator("body").innerText(),
          );
          throw error;
        } finally {
          release();
          await page.context().setOffline(false);
          await page.context().close();
        }
      }
  } finally {
    await writeFile(join(evidence, "fixture.log"), log);
    fixture.kill("SIGTERM");
  }
});
