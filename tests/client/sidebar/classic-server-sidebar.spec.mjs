import { test, expect } from "../playwright.mjs";
import { fixture } from "../servers/multi-server-fixture.mjs";

for (const viewport of [
  { width: 1440, height: 900 },
  { width: 390, height: 844 },
]) {
  test(`paired servers keep the full sidebar at ${viewport.width}px`, async ({
    page,
    context,
  }, testInfo) => {
    test.setTimeout(60000);
    page.setDefaultTimeout(10000);
    await page.setViewportSize(viewport);
    const colorScheme = viewport.width < 760 ? "light" : "dark";
    await page.emulateMedia({ colorScheme });
    const local = await fixture("Local");
    const remote = await fixture("Remote", true);
    local.snapshot.threads[0].pinned = true;
    remote.snapshot.threads[0].pinned = true;
    local.discoverPeer(remote);
    try {
      await context.addInitScript(
        ({ origin, destination }) => {
          const nativeFetch = window.fetch.bind(window);
          window.fetch = async (input, init) => {
            const request = new Request(input, init);
            const url = new URL(request.url);
            if (url.origin !== origin) return nativeFetch(request);
            const body = await request.clone().arrayBuffer();
            return nativeFetch(destination + url.pathname + url.search, {
              method: request.method,
              headers: request.headers,
              ...(body.byteLength ? { body } : {}),
              signal: request.signal,
              redirect: "error",
              credentials: "omit",
            });
          };
        },
        { origin: remote.invitation.origin, destination: remote.origin },
      );
      await page.goto(local.origin);
      await expect(page.getByLabel("Studio server")).toHaveValue("local");
      await expect(page.locator(".server-sidebar")).toHaveCount(0);
      const localFrame = page.frameLocator('iframe[title="Studio on Local"]');
      if (viewport.width < 760)
        await localFrame
          .getByRole("button", { name: "Toggle conversations" })
          .click();
      await expect(
        localFrame.getByRole("link", { name: "Codex Studio" }),
      ).toBeVisible();
      await expect(localFrame.locator("html")).toHaveAttribute(
        "data-mantine-color-scheme",
        colorScheme,
      );
      await expect(
        localFrame.getByLabel("Filter projects and chats"),
      ).toBeVisible();
      const localRow = localFrame
        .locator(".sidebar-row")
        .filter({ hasText: "Local chat" });
      await expect(localRow.locator(".chat-pin")).toBeVisible();
      await localRow.hover();
      await localRow
        .getByRole("button", { name: "Actions for Local chat" })
        .click();
      await expect(
        localFrame.getByRole("menuitem", { name: "Unpin", exact: true }),
      ).toBeVisible();
      await expect(
        localFrame.getByRole("menuitem", { name: "Move to folder" }),
      ).toBeVisible();
      await localFrame
        .getByRole("menuitem", { name: "Rename", exact: true })
        .click();
      await expect(
        localRow.getByRole("button", { name: "Save name" }),
      ).toBeVisible();
      await localRow
        .getByRole("button", { name: "Cancel", exact: true })
        .click();
      await page.screenshot({ path: testInfo.outputPath("classic-local.png") });

      await page.getByLabel("Studio server").selectOption("remote");
      const remoteFrame = page.frameLocator('iframe[title="Studio on Remote"]');
      if (viewport.width < 760)
        await remoteFrame
          .getByRole("button", { name: "Toggle conversations" })
          .click();
      const remoteBrand = remoteFrame.getByRole("link", {
        name: "Codex Studio",
      });
      await expect(remoteBrand).toBeVisible();
      await expect(remoteFrame.locator(".chat-pin")).toBeVisible();
      const ownedFrame = page.locator('iframe[title="Studio on Remote"]');
      const before = await ownedFrame.getAttribute("src");
      const brandURL = new URL(await remoteBrand.getAttribute("href"), before);
      expect(brandURL.href).toBe(before);
      await remoteBrand.click();
      await expect(remoteFrame.locator("html")).toHaveAttribute(
        "data-server-navigation",
        "classic",
      );
      if (viewport.width < 760)
        await remoteFrame
          .getByRole("button", { name: "Toggle conversations" })
          .click();
      await expect(
        remoteFrame.getByRole("button", { name: /Remote chat/ }).first(),
      ).toBeVisible();
      await page.screenshot({
        path: testInfo.outputPath("classic-remote.png"),
      });
      await page.getByLabel("Studio server").selectOption("local");
      await expect(
        localFrame.getByRole("link", { name: "Codex Studio" }),
      ).toBeVisible();
      expect(local.writes).toEqual([]);
      expect(remote.writes).toEqual([]);
    } finally {
      await remote.close();
      await local.close();
    }
  });
}
