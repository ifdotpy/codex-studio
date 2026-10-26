import { fixtureOrigin } from "../../../tests/misc-fixture.ts";
import { fileURLToPath } from "node:url";
import {
  test,
  expect,
  createFixtureViteServer,
} from "../../../tests/playwright.ts";

test("Workspace panel retains one form, scroll, focus, and modal ownership across hosts", async ({
  browser,
}, testInfo) => {
  const server = await createFixtureViteServer({
    configFile: false,
    root: fileURLToPath(new URL("../../..", import.meta.url)),
    server: { host: "127.0.0.1", port: 0 },
    plugins: [
      {
        name: "workspace-panel-fixture",
        configureServer(server) {
          server.middlewares.use((request, response, next) => {
            if (request.url !== "/panel-fixture") return next();
            response.setHeader("Content-Type", "text/html");
            response.end(
              '<!doctype html><meta name="viewport" content="width=device-width,initial-scale=1"><div id="root"></div><script type="module" src="/src/components/shell/WorkspacePanelHost.fixture.tsx"></script>',
            );
          });
        },
      },
    ],
  });
  const page = await browser.newPage({
    viewport: { width: 1440, height: 900 },
  });
  const errors: string[] = [];
  page.on("pageerror", (error) => errors.push(error.message));
  try {
    await server.listen();
    await page.goto(`${fixtureOrigin(server)}/panel-fixture`);
    const draft = page.getByRole("textbox", { name: "Draft" });
    await draft.fill("Keep this draft");
    await draft.evaluate((el: HTMLTextAreaElement) => {
      el.setSelectionRange(2, 6);
      window.originalInput = el;
      (
        el.parentElement ??
        (() => {
          throw new Error("Missing fixture scroll parent");
        })()
      ).scrollTop = 120;
    });
    await page.evaluate(() => window.changeMode("drawer"));
    await expect(page.getByRole("dialog", { name: "Workspace" })).toBeVisible();
    await expect(draft).toHaveValue("Keep this draft");
    await expect(draft).toBeFocused();
    expect(
      await draft.evaluate(
        (el: HTMLTextAreaElement) => el === window.originalInput,
      ),
    ).toBe(true);
    expect(
      await draft.evaluate((el: HTMLTextAreaElement) => [
        el.selectionStart,
        el.selectionEnd,
        (
          el.parentElement ??
          (() => {
            throw new Error("Missing fixture scroll parent");
          })()
        ).scrollTop,
      ]),
    ).toEqual([2, 6, 120]);
    await page.evaluate(() => window.changeMode("inline"));
    await expect(page.locator("#inline").getByLabel("Draft")).toBeVisible();
    await expect(page.getByRole("dialog")).toHaveCount(0);
    await expect(draft).toBeFocused();
    expect(await page.evaluate(() => window.mounts)).toBe(1);
    await page.getByRole("button", { name: "Conversation" }).click();
    await expect(
      page.getByRole("button", { name: "Conversation" }),
    ).toBeFocused();
    const resize = page.getByRole("separator", {
      name: "Workspace panel width",
    });
    await resize.focus();
    await resize.press("ArrowLeft");
    expect(await page.evaluate(() => window.savedWidth)).toBeGreaterThan(540);
    await resize.press("End");
    expect(await page.evaluate(() => window.savedWidth)).toBe(600);
    await page.screenshot({
      path: testInfo.outputPath("workspace-inline.png"),
    });
    await page.evaluate(() => window.changeMode("drawer"));
    await page.setViewportSize({ width: 390, height: 844 });
    await expect(page.getByRole("dialog", { name: "Workspace" })).toBeVisible();
    await expect(
      page.getByRole("dialog", { name: "Workspace" }),
    ).toBeInViewport({ ratio: 0.99 });
    await page.screenshot({
      path: testInfo.outputPath("workspace-drawer.png"),
    });
    await page.keyboard.press("Escape");
    await expect(draft).toBeHidden();
    await page.evaluate(() => window.setOpened(true));
    await expect(draft).toHaveValue("Keep this draft");
    expect(await page.evaluate(() => window.mounts)).toBe(1);
    await page.evaluate(() => window.changeMode("inline"));
    await expect(page.locator("#inline").getByLabel("Draft")).toBeVisible();
    await page.evaluate(() => window.setFormOpened(true));
    const formDraft = page.getByRole("textbox", { name: "Rule draft" });
    await formDraft.fill("Keep the rule draft");
    await page.evaluate(() => window.changeMode("drawer"));
    await expect(
      page.getByRole("dialog", { name: "Workspace" }),
    ).toBeInViewport({ ratio: 0.99 });
    await expect(formDraft).toBeFocused();
    await expect(formDraft).toHaveValue("Keep the rule draft");
    await formDraft.press("Tab");
    await expect(
      page
        .getByRole("dialog", { name: "Rule form" })
        .getByRole("button", { name: "Close rule" }),
    ).toBeFocused();
    await page.evaluate(() => window.changeMode("inline"));
    await expect(formDraft).toHaveValue("Keep the rule draft");
    expect(await page.evaluate(() => window.mounts)).toBe(1);
    expect(errors).toEqual([]);
  } finally {
    await page.close();
    await server.close();
  }
});
