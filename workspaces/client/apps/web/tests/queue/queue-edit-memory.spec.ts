import { fileURLToPath } from "node:url";
import type { AddressInfo } from "node:net";
import { expect, test } from "../playwright";

test("queue edits stay in memory and save on user action", async ({
  context,
}) => {
  const { createFixtureViteServer } = await import("../playwright");
  const server = await createFixtureViteServer({
    configFile: false,
    root: fileURLToPath(new URL("../../", import.meta.url)),
    server: { host: "127.0.0.1", port: 0 },
  });
  await server.listen();
  try {
    const page = await context.newPage();
    await page.route("**/check", (route) =>
      route.fulfill({
        contentType: "text/html",
        body: "<!doctype html><title>fixture</title>",
      }),
    );
    const origin = `http://127.0.0.1:${(server.httpServer!.address() as AddressInfo).port}`;
    const mount = async () => {
      await page.goto(origin + "/check");
      await page.evaluate(async () => {
        const fixture =
          await import("/tests/queue/queue-edit-memory.fixture.tsx");
        fixture.mount();
      });
    };
    await mount();
    await page.getByRole("button", { name: "Edit queued message 1" }).click();
    await page
      .getByRole("textbox", { name: "Edit queued message" })
      .fill("changed");
    expect(await page.evaluate(() => localStorage.length)).toBe(0);
    await mount();
    await expect(
      page.getByRole("textbox", { name: "Edit queued message" }),
    ).toHaveCount(0);
    await page.getByRole("button", { name: "Edit queued message 1" }).click();
    await page
      .getByRole("textbox", { name: "Edit queued message" })
      .fill("saved");
    await page.getByRole("button", { name: "Save queued message" }).click();
    await expect(page).toHaveTitle("saved");
  } finally {
    await server.close();
  }
});
