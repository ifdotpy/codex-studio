import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { once } from "node:events";
import {
  test,
  expect,
  spawnFixture,
  readTestState,
  rustUiFixture,
} from "../playwright.mjs";
import { waitForFixturePort } from "../fixture-readiness.mjs";
import { fixtureStateDirectory } from "../fixtures/rust-ui-fixture.mjs";

test("App keeps confirmed deleted chats unreadable when HTTP wins before SSE", async ({
  page,
}) => {
  const root = await mkdtemp(join(tmpdir(), "project-delete-app-"));
  const child = spawnFixture(
    process.execPath,
    [rustUiFixture, root, "--workers", "0"],
    { stdio: ["pipe", "pipe", "pipe"] },
  );
  let log = "";
  child.stderr.on("data", (chunk) => {
    log += chunk;
  });
  try {
    const port = await waitForFixturePort(child, { diagnostics: () => log });
    const origin = `http://127.0.0.1:${port}`;
    const state = await readTestState(origin);
    const ids = state.runtime.agents
      .filter((agent) => !agent.parentId)
      .map((agent) => agent.id);
    const project = state.runtime.projects.find(
      (project) => project.path === state.runtime.agents[0].cwd,
    );
    let deletes = 0;
    await page.route("**/api/projects/delete", async (route) => {
      deletes++;
      const request = route.request().postDataJSON();
      // Deliberately hold the backend's entity baseline unchanged after the receipt.
      await route.fulfill({
        json: {
          id: request.id,
          request_id: request.request_id,
          outcome: "deleted",
          deletedAgentIds: ids,
          deletedRoomIds: [],
        },
      });
    });
    await page.setViewportSize({ width: 1440, height: 960 });
    await page.goto(origin);
    await expect(page.locator("#message")).toBeVisible();
    const options = page.getByRole("button", {
      name: `Options for project ${project.name || project.path}`,
    });
    await options.focus();
    await page.keyboard.press("Enter");
    await page.getByRole("menuitem", { name: "Delete project…" }).click();
    await page
      .getByRole("dialog", { name: "Delete project", exact: true })
      .getByRole("button", { name: "Delete project", exact: true })
      .click();
    await expect(
      page.getByRole("dialog", { name: "Delete project", exact: true }),
    ).toHaveCount(0);
    expect(deletes).toBe(1);
    await page.evaluate(
      () =>
        new Promise((resolve) =>
          requestAnimationFrame(() => requestAnimationFrame(resolve)),
        ),
    );
    await expect(
      page.getByRole("heading", { name: "New conversation", exact: true }),
    ).toBeVisible();
    for (const id of ids) {
      const row = page.locator(`[data-chat="${id}"]`);
      await expect(row).toHaveAttribute("aria-current", "false");
      await row.click();
      await expect(
        page.getByRole("heading", { name: "New conversation", exact: true }),
      ).toBeVisible();
    }
    expect(
      (await readTestState(origin)).runtime.agents.filter((agent) =>
        ids.includes(agent.id),
      ),
    ).toHaveLength(ids.length);
  } finally {
    if (child.exitCode === null) {
      const exited = once(child, "close");
      child.kill("SIGTERM");
      await exited;
    }
    await rm(root, { recursive: true, force: true });
    await rm(fixtureStateDirectory(root), { recursive: true, force: true });
  }
});
