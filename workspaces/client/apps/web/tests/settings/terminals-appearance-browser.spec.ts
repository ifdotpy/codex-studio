import { test, expect } from "../playwright.ts";
import { fixture } from "../servers/multi-server-fixture.ts";
import { remoteNetwork } from "../servers/browser-fixture.ts";
import {
  defaultStudioPreferences,
  studioPreferencesStorageKey,
} from "../../src/studioPreferences.ts";

for (const navigation of ["standalone", "combined"]) {
  test(`Appearance hides and restores terminals across reloads in ${navigation}`, async ({
    page,
    context,
  }) => {
    const server = await fixture("Local");
    const remote =
      navigation === "combined" ? await fixture("Remote", true) : null;
    if (remote) await remoteNetwork(context, remote);
    server.snapshot.runtime.terminals = [
      {
        id: "running-terminal",
        agent: "overlap",
        title: "Running session",
        cwd: "/same/project",
        created: 1,
        status: "running",
      },
    ];
    const terminalWrites: string[] = [];
    page.on("request", (request) => {
      const path = new URL(request.url()).pathname;
      if (request.method() === "POST" && path.startsWith("/api/terminals/"))
        terminalWrites.push(path);
    });
    const { showTerminals: _, ...legacy } = defaultStudioPreferences;
    await page.addInitScript(
      ({ key, value }) => {
        if (!localStorage.getItem(key))
          localStorage.setItem(key, JSON.stringify(value));
      },
      { key: studioPreferencesStorageKey, value: legacy },
    );
    try {
      await page.goto(server.origin);
      if (remote) {
        await page
          .getByRole("button", { name: "Studio settings", exact: true })
          .click();
        const settings = page.getByRole("dialog", {
          name: "Studio settings",
          exact: true,
        });
        await settings
          .getByRole("tab", { name: "Servers", exact: true })
          .click();
        await settings
          .getByRole("button", { name: "Add with an invitation", exact: true })
          .click();
        await settings
          .getByRole("textbox", { name: "Server address" })
          .fill(remote.invitation.origin);
        await settings
          .getByRole("textbox", { name: "Pairing invitation" })
          .fill(JSON.stringify(remote.invitation));
        await settings
          .getByRole("button", { name: "Pair server", exact: true })
          .click();
        await expect(page.locator(".multi-server-shell")).toBeVisible();
        await settings
          .getByRole("button", { name: "Close", exact: true })
          .click();
        await page.getByRole("button", { name: /^Local chat/ }).click();
      }
      const dock = page.getByRole("region", { name: "Terminals", exact: true });
      const shell = page.locator(".multi-server-shell");
      const settings = page.getByRole("dialog", {
        name: "Studio settings",
        exact: true,
      });
      const openAppearance = async () => {
        await page
          .getByRole("button", { name: "Studio settings", exact: true })
          .click();
        await settings
          .getByRole("tab", { name: "Appearance", exact: true })
          .click();
      };
      const toggle = settings.getByRole("checkbox", {
        name: "Show terminals",
        exact: true,
      });
      await expect(dock).toHaveCount(1);
      await openAppearance();
      await expect(toggle).toBeChecked();
      await toggle.uncheck();
      await settings
        .getByRole("button", { name: "Close", exact: true })
        .click();
      await expect(dock).toHaveCount(0);
      await expect(page.locator(".shell-terminal-bar")).toHaveCount(0);
      const expectNoGap = async () => {
        const height = await page.evaluate(() => window.innerHeight);
        await expect(page.locator("#root")).toHaveCSS("height", `${height}px`);
        if (navigation === "combined")
          await expect(
            page.locator(".multi-server-shell > #sidebar"),
          ).toHaveCSS("height", `${height}px`);
      };
      await expectNoGap();
      await openAppearance();
      await toggle.check();
      await settings
        .getByRole("button", { name: "Close", exact: true })
        .click();
      await expect(dock).not.toHaveClass(/is-open/);
      if (navigation === "combined") {
        await expect(page.locator(".shell-terminal-bar")).toBeVisible();
        await expect(shell).toHaveAttribute("data-shell-terminal", "closed");
      }
      await dock
        .getByRole("button", { name: "Show terminals", exact: true })
        .click();
      if (navigation === "combined")
        await expect(shell).toHaveAttribute("data-shell-terminal", "open");
      await expect(dock).toHaveClass(/is-open/);
      await expect(
        dock.getByText("Running session", { exact: true }),
      ).toBeVisible();

      await openAppearance();
      await expect(toggle).toBeChecked();
      const savedFalse = page.waitForResponse((response) => {
        const request = response.request();
        return (
          new URL(response.url()).pathname === "/api/sync/preferences" &&
          request.method() === "POST" &&
          request.postData()?.includes('"showTerminals') === true &&
          request.postData()?.includes('"value":false') === true
        );
      });
      await toggle.uncheck();
      expect((await savedFalse).ok()).toBe(true);
      await expect(dock).toHaveCount(0);
      await expect(page.locator(".shell-terminal-bar")).toHaveCount(0);
      await expectNoGap();
      await expect(page.locator("html")).not.toHaveAttribute(
        "data-terminal-dock",
      );
      expect(
        await page
          .locator("html")
          .evaluate((root) =>
            root.style.getPropertyValue("--terminal-dock-height"),
          ),
      ).toBe("");
      if (navigation === "combined") {
        await expect(shell).not.toHaveAttribute("data-shell-terminal");
        await expect(shell).toHaveCSS("--shell-terminal-height", "0px");
      }
      await page.reload();
      await openAppearance();
      await expect(toggle).not.toBeChecked();
      await expect(dock).toHaveCount(0);
      await expect(page.locator(".shell-terminal-bar")).toHaveCount(0);
      await toggle.check();
      await settings
        .getByRole("button", { name: "Close", exact: true })
        .click();
      await expect(dock).toHaveClass(/is-open/);
      await expect(
        dock.getByText("Running session", { exact: true }),
      ).toBeVisible();
      if (navigation === "combined")
        await expect(shell).toHaveAttribute("data-shell-terminal", "open");
      await page.reload();
      await expect(dock).toHaveClass(/is-open/);
      expect(terminalWrites).toEqual([]);
    } finally {
      await remote?.close();
      await server.close();
    }
  });
}
