import { test, expect } from "../playwright.mjs";
import { fixture } from "./multi-server-fixture.mjs";

test("direct screens keep overlapping chat drafts and sends with their owner", async ({
  page,
  context,
}, testInfo) => {
  const local = await fixture("Local");
  const remote = await fixture("Remote", true);
  const errors = [];
  let quotaFault = false;
  page.on("pageerror", (error) => {
    if (quotaFault && error.name === "QuotaExceededError") return;
    errors.push(error.message);
  });
  try {
    await context.addInitScript(
      ({ origin, destination }) => {
        const fetchRequest = window.fetch.bind(window);
        window.fetch = async (input, init) => {
          const request = new Request(input, init);
          const url = new URL(request.url);
          if (url.origin !== origin) return fetchRequest(request);
          const bytes = await request.clone().arrayBuffer();
          return fetchRequest(destination + url.pathname + url.search, {
            method: request.method,
            headers: request.headers,
            ...(bytes.byteLength ? { body: bytes } : {}),
            signal: request.signal,
            redirect: "error",
            credentials: "omit",
          });
        };
      },
      { origin: remote.invitation.origin, destination: remote.origin },
    );
    await page.goto(local.origin);
    await expect(page.locator("#message")).toBeVisible();
    await page
      .getByRole("button", { name: "Studio settings", exact: true })
      .click();
    await page.getByRole("tab", { name: "Servers", exact: true }).click();
    const dialog = page.getByRole("dialog", { name: "Studio settings" });
    await dialog
      .getByRole("button", { name: "Add with an invitation", exact: true })
      .click();
    await dialog.getByLabel("Server address").fill(remote.invitation.origin);
    await dialog
      .getByLabel("Pairing invitation")
      .fill(JSON.stringify(remote.invitation));
    await dialog
      .getByRole("button", { name: "Pair server", exact: true })
      .click();
    const chat = (alias) =>
      page.locator("#chat-list .chat-row").filter({
        has: page.locator(`.chat-server-line[aria-label$=", ${alias}"]`),
      });
    await expect(chat("REM")).toContainText("Remote chat");
    await dialog.getByRole("button", { name: "Close", exact: true }).click();
    await expect(page.locator("iframe")).toHaveCount(0);
    await chat("LOC").click();
    await expect(page.locator("#message")).toHaveCount(1);
    await page.locator("#message").fill("local draft");
    await chat("REM").click();
    await page.locator("#message").fill("remote draft");
    await chat("LOC").click();
    await expect(page.locator("#message")).toHaveValue("local draft");
    await chat("REM").click();
    await expect(page.locator("#message")).toHaveValue("remote draft");
    await page
      .getByRole("button", { name: "Send message", exact: true })
      .click();
    await expect.poll(() => remote.writes.length).toBe(1);
    expect(remote.writes[0].text).toBe("remote draft");
    expect(local.writes).toHaveLength(0);
    quotaFault = true;
    await page.evaluate(() => {
      const original = Storage.prototype.setItem;
      Storage.prototype.setItem = function (key, value) {
        if (
          key.includes("codex-chat-draft:") ||
          (key.includes("codex-drafts:") && key.includes(":pending:"))
        )
          throw new DOMException("Full fixture storage", "QuotaExceededError");
        original.call(this, key, value);
      };
      for (const method of ["put", "add"]) {
        const originalMethod = IDBObjectStore.prototype[method];
        IDBObjectStore.prototype[method] = function (...args) {
          if (this.transaction.db.name.endsWith("--drafts"))
            throw new DOMException(
              "Full fixture storage",
              "QuotaExceededError",
            );
          return originalMethod.apply(this, args);
        };
      }
    });
    await page.locator("#message").fill("unsaved remote draft");
    await expect(page.locator("[data-draft-sync-status]")).toContainText(
      "Keep this chat open",
    );
    await chat("LOC").click();
    await expect(page.locator("#message")).toHaveValue("local draft");
    await chat("REM").click();
    await expect(page.locator("#message")).toHaveValue("unsaved remote draft");
    await page.screenshot({
      path: testInfo.outputPath("direct-server-screens.png"),
    });
    remote.mismatchSchema();
    await expect(
      page.getByRole("main", { name: "Server update required" }),
    ).toBeVisible();
    await expect(page.getByRole("alertdialog")).toHaveCount(0);
    await expect(page.locator("#message")).toHaveCount(0);
    await chat("LOC").click();
    await expect(page.locator("#message")).toHaveValue("local draft");
    expect(remote.writes).toHaveLength(1);
    expect(errors).toEqual([]);
  } finally {
    await Promise.all([local.close(), remote.close()]);
  }
});
