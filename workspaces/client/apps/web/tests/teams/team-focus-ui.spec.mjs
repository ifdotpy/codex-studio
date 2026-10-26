import {
  test,
  expect,
  spawnFixture,
  rustUiFixture,
  readTestState,
  stubEntityState,
  readFixtureSyncContract,
} from "../playwright.ts";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

for (const width of [1024, 390]) {
  test(`Team drawer cycles focus through closed and open disclosures at ${width}`, async ({
    page,
  }) => {
    test.setTimeout(60000);
    const state = await mkdtemp(join(tmpdir(), "team-focus-"));
    const fixture = spawnFixture(process.execPath, [rustUiFixture, state], {
      stdio: ["ignore", "pipe", "pipe"],
    });
    const port = await new Promise((resolve, reject) => {
      fixture.stdout.once("data", (chunk) =>
        resolve(Number(String(chunk).trim())),
      );
      fixture.once("exit", () => reject(new Error("Fixture exited")));
    });
    const origin = `http://127.0.0.1:${port}`;
    const snapshot = await readTestState(origin);
    const lead = snapshot.threads.find((a) => a.name === "Release lead");
    lead.overview = {
      result: "Lead report",
      resultTruncated: true,
      resultFile: "lead.md",
    };
    const workers = snapshot.threads.filter(
      (a) => a.rootId === lead.id && !a.isLead,
    );
    workers.forEach((worker, index) => {
      worker.status = index === 0 ? "failed" : "completed";
      worker.inFlight = false;
      worker.overview = { result: "Worker report", resultTruncated: true };
      if (index === 0) {
        worker.error = "Fixture error";
        worker.commandProgress = [
          {
            monitorId: "focus-command",
            stage: "Check focus",
            startedAt: Date.now() / 1000,
          },
        ];
      }
    });
    snapshot.runtime.agents = snapshot.threads;
    await stubEntityState(
      page,
      snapshot,
      await readFixtureSyncContract(origin),
    );
    await page.setViewportSize({ width: 1440, height: 900 });
    await page.goto(origin);
    await page.locator(`.project-chats [data-chat="${lead.id}"]`).click();
    await page.setViewportSize({ width, height: 900 });
    await page
      .getByRole("button", { name: "Chat actions", exact: true })
      .click();
    await page.getByRole("menuitem", { name: "Team", exact: true }).click();
    const dialog = page.getByRole("dialog", { name: "Team", exact: true });
    await expect(dialog).toBeVisible();
    const close = dialog.getByRole("button", { name: "Close team" });
    const cycle = async (key, expectedWorkers) => {
      await close.focus();
      let wrapped = false;
      const seen = new Set();
      const visitedWorkers = new Set();
      for (let step = 0; step < 100; step++) {
        await page.keyboard.press(key);
        const focus = await dialog.evaluate((node) => {
          const active = document.activeElement;
          return {
            inside: node.contains(active),
            worker: active
              ?.closest("[data-worker]")
              ?.getAttribute("data-worker"),
            label: node.contains(active)
              ? active?.getAttribute("aria-label") ||
                active?.textContent?.trim()
              : active?.tagName,
            close: active?.id === "team-close",
          };
        });
        expect(
          focus.inside,
          `${key} must stay in Team (focused ${focus.label})`,
        ).toBe(true);
        if (focus.close) {
          wrapped = true;
          break;
        }
        seen.add(focus.label);
        if (focus.worker) visitedWorkers.add(focus.worker);
      }
      expect(visitedWorkers.size).toBe(expectedWorkers);
      expect([...seen].some((label) => label?.includes("Last report"))).toBe(
        true,
      );
      expect(seen.has("Command progress")).toBe(true);
      expect(seen.has("Error details")).toBe(true);
      expect(wrapped, `${key} returns to Close team`).toBe(true);
      expect([...seen].some((label) => label?.includes("Finished"))).toBe(true);
      expect(
        [...seen].some((label) => label?.includes("Main agent report")),
      ).toBe(true);
    };
    await cycle("Tab", 1);
    await cycle("Shift+Tab", 1);
    await dialog.getByText("Main agent report", { exact: true }).click();
    await dialog
      .locator(".worker-command-details")
      .getByRole("button", { name: "Command progress", exact: true })
      .click();
    await dialog
      .locator(".worker-excerpt")
      .first()
      .getByText("Last report", { exact: true })
      .click();
    await dialog
      .locator(".worker-error-details")
      .getByRole("button", { name: "Error details", exact: true })
      .click();
    await dialog
      .getByLabel(`Finished (${workers.length - 1})`, { exact: true })
      .click();
    await expect(
      dialog.getByRole("button", { name: "Continue in main chat" }),
    ).toBeVisible();
    await expect(
      dialog.getByText("Check focus", { exact: false }).last(),
    ).toBeVisible();
    await cycle("Tab", workers.length);
    await cycle("Shift+Tab", workers.length);
    await page.keyboard.press("Escape");
    await expect(dialog).toBeHidden();
  });
}
