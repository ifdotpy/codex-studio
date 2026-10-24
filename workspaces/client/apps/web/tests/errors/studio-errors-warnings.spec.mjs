import { mkdir } from "node:fs/promises";
import { test, expect } from "../playwright.mjs";
import { fixture } from "../servers/multi-server-fixture.mjs";

for (const width of [1440, 390]) {
  for (const colorScheme of ["light", "dark"]) {
    test(`App errors in sidebar warnings ${width} ${colorScheme}`, async ({
      page,
    }) => {
      await page.setViewportSize({ width, height: 900 });
      await page.emulateMedia({ colorScheme });
      let items = ["one", "two", "three"].map((id) => ({
        id: `monitor:${id}`,
        kind: "monitor_outcome_lost",
        reason: "Studio lost the command result.",
        action: "check",
        agentId: "overlap",
        agentName: `Worker ${id}`,
        monitorId: id,
        eventId: null,
        detail: "The command may have run. It will not be repeated.",
        subject: "cargo test",
        at: 1792700000,
      }));
      const dismissed = [];
      const server = await fixture("Local", false, {
        handle({ url, body, json }) {
          if (url.pathname === "/api/studio-errors") {
            json({ items });
            return true;
          }
          if (url.pathname === "/api/studio-errors/dismiss") {
            dismissed.push(body.ids);
            items = items.filter((item) => !body.ids.includes(item.id));
            json({ items, dismissedIds: body.ids, unknownIds: [] });
            return true;
          }
        },
      });
      server.snapshot.runtime.nativeNotices = [
        {
          id: "provider-update:default",
          accountKey: "default",
          provider: "codex",
          message: "A newer Codex version is available.",
          level: "info",
          at: 1,
        },
      ];
      try {
        await page.goto(server.origin);
        const header = page.locator("#sidebar .sidebar-header");
        await expect(header).toBeVisible();
        const trigger = header.getByRole("button", {
          name: "Warnings",
          exact: true,
        });
        await expect(trigger).toBeVisible();
        await expect(
          header.getByRole("button", { name: "Studio settings", exact: true }),
        ).toBeVisible();
        await expect(
          page.getByRole("button", { name: /^Studio errors/ }),
        ).toHaveCount(0);
        await trigger.focus();
        await page.keyboard.press("Enter");
        const dialog = page.getByRole("dialog", {
          name: "Warnings",
          exact: true,
        });
        await expect(
          dialog.getByRole("tab", { name: /Notices/ }),
        ).toBeVisible();
        await expect(
          dialog.getByText("A newer Codex version is available."),
        ).toBeVisible();
        await dialog.getByRole("tab", { name: /App errors/ }).click();
        const remove = dialog.getByRole("button", { name: "Delete selected" });
        await expect(remove).toBeDisabled();
        await dialog.getByRole("button", { name: "Select all" }).click();
        await expect(
          dialog.getByRole("checkbox", { checked: true }),
        ).toHaveCount(3);
        await dialog
          .getByRole("checkbox", {
            name: "Select error monitor:two for Worker two",
          })
          .uncheck();
        await mkdir(test.info().outputPath("screenshots"), { recursive: true });
        await page.screenshot({
          path: test
            .info()
            .outputPath("screenshots", `${width}-${colorScheme}.png`),
          animations: "disabled",
        });
        await remove.click();
        await expect(dialog.getByRole("checkbox")).toHaveCount(1);
        expect(dismissed).toEqual([["monitor:one", "monitor:three"]]);
        expect(items.map((item) => item.id)).toEqual(["monitor:two"]);
        await expect(remove).toBeDisabled();
        await dialog.getByRole("tab", { name: /Notices/ }).click();
        await expect(
          dialog.getByText("A newer Codex version is available."),
        ).toBeVisible();
      } finally {
        await server.close();
      }
    });
  }
}
