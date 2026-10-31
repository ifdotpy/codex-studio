import { once } from "node:events";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  expect,
  readTestState,
  rustUiFixture,
  spawnFixture,
  test,
} from "../playwright.ts";
import { fixtureStateDirectory } from "../fixtures/rust-ui-fixture.ts";

test("draft baseline restores reloads, converges across tabs, and shows conflicts", async ({
  browser,
}) => {
  test.setTimeout(90_000);
  const state = await mkdtemp(join(tmpdir(), "draft-baseline-"));
  const fixture = spawnFixture(process.execPath, [rustUiFixture, state], {
    stdio: ["pipe", "pipe", "pipe"],
  });
  let log = "";
  fixture.stderr!.on("data", (chunk) => {
    log += String(chunk);
  });
  const context = await browser.newContext();
  try {
    const port = await new Promise<number>((resolve, reject) => {
      fixture.stdout!.once("data", (chunk) =>
        resolve(Number(String(chunk).trim())),
      );
      fixture.once("exit", () => reject(new Error(log)));
    });
    const origin = `http://127.0.0.1:${port}`;
    const snapshot = await readTestState(origin);
    const agent = snapshot.runtime.agents.find(
      (item) => item.name === "Other project",
    );
    if (!agent) throw new Error("fixture chat missing");
    const first = await context.newPage();
    const second = await context.newPage();
    const draftPulls: string[] = [];
    for (const page of [first, second]) {
      page.on("request", (request) => {
        const url = new URL(request.url());
        if (
          url.pathname === "/api/sync/pull" &&
          url.searchParams.get("scope") === "drafts"
        )
          draftPulls.push(request.url());
      });
      await page.goto(origin);
      await page
        .locator("[data-chat]")
        .filter({ hasText: "Other project" })
        .click();
    }
    const saved = first.waitForResponse(
      (response) =>
        response.request().method() === "POST" &&
        new URL(response.url()).pathname === "/api/sync/drafts" &&
        response.ok(),
    );
    await first.locator("#message").fill("Saved draft");
    await saved;
    await expect(second.locator("#message")).toHaveValue("Saved draft");
    await first.reload();
    await expect(first.locator("#message")).toHaveValue("Saved draft");
    const secondSaved = second.waitForResponse(
      (response) =>
        response.request().method() === "POST" &&
        new URL(response.url()).pathname === "/api/sync/drafts" &&
        response.ok(),
    );
    await second.locator("#message").fill("Continued in another tab");
    await secondSaved;
    await expect(first.locator("#message")).toHaveValue(
      "Continued in another tab",
    );
    const identity = await (await fetch(origin + "/api/sync/identity")).json();
    const branch = {
      id: `offline:${agent.id}`,
      session: agent.id,
      device: "offline",
      text: "Concurrent text",
      updated: 1,
    };
    const response = await fetch(origin + "/api/sync/drafts", {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        "X-Canvas-Token": snapshot.token,
        "X-Canvas-Workspace": identity.workspaceId,
        Origin: origin,
      },
      body: JSON.stringify({
        rows: [
          {
            newDocumentState: {
              id: branch.id,
              payload: JSON.stringify(branch),
              deleted: false,
            },
          },
        ],
      }),
    });
    expect(response.ok).toBe(true);
    await expect(
      first.getByRole("button", { name: /^Other drafts/ }),
    ).toBeVisible();
    await expect(first.locator("#message")).toHaveValue(
      "Continued in another tab",
    );
    expect(draftPulls).toEqual([]);
  } finally {
    await context.close();
    if (fixture.exitCode === null && fixture.signalCode === null) {
      const exited = once(fixture, "exit", {
        signal: AbortSignal.timeout(10_000),
      });
      fixture.kill("SIGTERM");
      await exited;
    }
    await rm(state, { recursive: true, force: true });
    await rm(fixtureStateDirectory(state), { recursive: true, force: true });
  }
});
