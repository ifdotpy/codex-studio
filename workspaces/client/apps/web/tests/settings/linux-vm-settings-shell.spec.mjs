import { test, expect } from "../playwright.mjs";
import { fixture } from "../servers/multi-server-fixture.mjs";

const linuxVmConfiguration = {
  settings: {
    cpus: 4,
    memoryBytes: 4294967296,
    systemDiskBytes: 17179869184,
    dataDiskBytes: 137438953472,
  },
  state: "stopped",
  allocatedDiskBytes: null,
};

test("settings tabs stay in the direct app when there are no paired servers", async ({
  page,
}) => {
  page.setDefaultTimeout(12000);
  const local = await fixture("Local");
  try {
    await page.route("**/api/linux-vm/settings", (route) =>
      route.fulfill({ json: linuxVmConfiguration }),
    );
    await page.goto(local.origin + "/?studio-navigation=classic");
    await page.locator("#message").waitFor();
    const settingsButton = page.getByRole("button", {
      name: "Studio settings",
      exact: true,
    });
    await settingsButton.click();
    const settings = page.getByRole("dialog", {
      name: "Studio settings",
      exact: true,
    });
    await expect(settings).toBeVisible();
    await page.evaluate(() => {
      window.__workspaceBeforeSettingsNavigation =
        document.querySelector("main.workspace");
    });

    for (const name of [
      "Accounts",
      "Appearance",
      "Federation",
      "Servers",
      "Linux VM",
      "Servers",
      "Federation",
      "Appearance",
      "Accounts",
    ]) {
      const tab = settings.getByRole("tab", { name, exact: true });
      await tab.click();
      await expect(settings).toBeVisible();
      await expect(tab).toHaveAttribute("aria-selected", "true");
      if (name === "Linux VM")
        await expect(
          settings.getByText("VM state: stopped", { exact: true }),
        ).toBeVisible();
      if (name === "Servers")
        await expect(
          settings.getByRole("region", { name: "Servers", exact: true }),
        ).toBeVisible();
      expect(
        await page.evaluate(
          () =>
            window.__workspaceBeforeSettingsNavigation ===
            document.querySelector("main.workspace"),
        ),
        `App's workspace element should survive selecting ${name}`,
      ).toBe(true);
    }
  } finally {
    await local.close();
  }
});

test("paired-server settings still hand off between shell and local frame", async ({
  page,
}) => {
  page.setDefaultTimeout(12000);
  const local = await fixture("Local");
  try {
    await page.route("**/api/linux-vm/settings", (route) =>
      route.fulfill({ json: linuxVmConfiguration }),
    );
    await page.addInitScript(() => {
      localStorage.setItem(
        "studio-paired-servers-v1",
        JSON.stringify([
          {
            id: "remote",
            label: "Remote",
            origin: "https://remote.tailnet.ts.net",
            credentialId: "fixture-credential",
          },
        ]),
      );
    });
    await page.goto(local.origin + "/?studio-navigation=classic");
    const localFrame = page.frameLocator('iframe[src*="studio-server=local"]');
    await expect(localFrame.locator("#message")).toBeVisible();
    await page
      .getByRole("combobox", { name: "Studio server" })
      .selectOption("local");
    const settings = page.getByRole("dialog", {
      name: "Studio settings",
      exact: true,
    });
    await page
      .getByRole("button", { name: "Studio settings", exact: true })
      .click();
    await expect(settings).toBeVisible();
    await settings.getByRole("tab", { name: "Linux VM", exact: true }).click();
    await expect(settings).toBeHidden();
    const frameSettings = localFrame.getByRole("dialog", {
      name: "Studio settings",
      exact: true,
    });
    await expect(frameSettings).toBeVisible();
    await expect(
      frameSettings.getByRole("tab", { name: "Linux VM", exact: true }),
    ).toHaveAttribute("aria-selected", "true");

    await frameSettings
      .getByRole("tab", { name: "Servers", exact: true })
      .click();
    await expect(frameSettings).toBeHidden();
    await expect(settings).toBeVisible();
    await expect(
      settings.getByRole("tab", { name: "Servers", exact: true }),
    ).toHaveAttribute("aria-selected", "true");
  } finally {
    await local.close();
  }
});

test("settings follow paired-server topology changes without stale reopening", async ({
  page,
}) => {
  page.setDefaultTimeout(12000);
  const local = await fixture("Local");
  const remote = {
    id: "remote",
    label: "Remote",
    origin: "https://remote.tailnet.ts.net",
    credentialId: "fixture-credential",
  };
  try {
    await page.goto(local.origin + "/?studio-navigation=classic");
    await page.locator("#message").waitFor();
    await page
      .getByRole("button", { name: "Studio settings", exact: true })
      .click();
    const settings = page.getByRole("dialog", {
      name: "Studio settings",
      exact: true,
    });
    await expect(settings).toBeVisible();
    await settings.getByRole("tab", { name: "Servers", exact: true }).click();
    await expect(
      settings.getByRole("tab", { name: "Servers", exact: true }),
    ).toHaveAttribute("aria-selected", "true");
    await page.evaluate((server) => {
      localStorage.setItem(
        "studio-paired-servers-v1",
        JSON.stringify([server]),
      );
      window.dispatchEvent(new Event("studio-server-registry"));
    }, remote);
    await expect(
      page.getByRole("combobox", { name: "Studio server" }),
    ).toHaveValue("local");
    await expect(settings).toBeVisible();
    await expect(
      settings.getByRole("tab", { name: "Servers", exact: true }),
    ).toHaveAttribute("aria-selected", "true");
    await settings.getByRole("button", { name: "Remove from this UI" }).click();

    const directSettings = page.getByRole("dialog", {
      name: "Studio settings",
      exact: true,
    });
    await expect(directSettings).toBeVisible();
    await expect(
      directSettings.getByRole("region", { name: "Servers", exact: true }),
    ).toBeVisible();
    await directSettings.getByRole("button", { name: "Close" }).click();
    await expect(directSettings).toBeHidden();

    await page.evaluate((server) => {
      localStorage.setItem(
        "studio-paired-servers-v1",
        JSON.stringify([server]),
      );
      window.dispatchEvent(new Event("studio-server-registry"));
    }, remote);
    await expect(
      page.getByRole("dialog", { name: "Studio settings", exact: true }),
    ).toBeHidden();
  } finally {
    await local.close();
  }
});

test("reopening shell settings resets a frame-owned tab to Servers", async ({
  page,
}) => {
  page.setDefaultTimeout(12000);
  const local = await fixture("Local");
  try {
    await page.route("**/api/linux-vm/settings", (route) =>
      route.fulfill({ json: linuxVmConfiguration }),
    );
    await page.addInitScript(() => {
      localStorage.setItem(
        "studio-paired-servers-v1",
        JSON.stringify([
          {
            id: "remote",
            label: "Remote",
            origin: "https://remote.tailnet.ts.net",
            credentialId: "fixture-credential",
          },
        ]),
      );
    });
    await page.goto(local.origin + "/?studio-navigation=classic");
    await page
      .getByRole("combobox", { name: "Studio server" })
      .selectOption("local");
    const localFrame = page.frameLocator('iframe[src*="studio-server=local"]');
    await expect(localFrame.locator("#message")).toBeVisible();
    await page
      .getByRole("button", { name: "Studio settings", exact: true })
      .click();
    const shellSettings = page.getByRole("dialog", {
      name: "Studio settings",
      exact: true,
    });
    await expect(shellSettings).toBeVisible();
    await shellSettings
      .getByRole("tab", { name: "Linux VM", exact: true })
      .click();
    await expect(shellSettings).toBeHidden();
    const frameSettings = localFrame.getByRole("dialog", {
      name: "Studio settings",
      exact: true,
    });
    await expect(frameSettings).toBeVisible();

    await page
      .getByRole("button", { name: "Studio settings", exact: true })
      .click();
    await expect(shellSettings).toBeVisible();
    await expect(
      shellSettings.getByRole("tab", { name: "Servers", exact: true }),
    ).toHaveAttribute("aria-selected", "true");
    await expect(
      shellSettings.getByRole("region", { name: "Servers", exact: true }),
    ).toBeVisible();
  } finally {
    await local.close();
  }
});
