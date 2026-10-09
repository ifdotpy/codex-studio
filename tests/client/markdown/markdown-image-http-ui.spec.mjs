import {
  apiSchemaHeaders,
  entityPullFixture,
  playwrightScratchRoot,
  readFixtureSyncContract,
  readTestState,
  stubEntityState,
  test,
  spawnFixture as spawn,
} from "../playwright.mjs";
// Production HTTP policy and renderer. External image responses are isolated fixtures.
import assert from "node:assert/strict";
import { mkdtemp } from "node:fs/promises";
import { join } from "node:path";
test("markdown image http ui", async ({ page: runnerPage }) => {
  const repo = new URL("../../../", import.meta.url).pathname;
  const root = await mkdtemp(join(playwrightScratchRoot, "studio-image-http-"));
  const fixture = spawn("python3", [
    "-B",
    join(repo, "tests/simple-ui-fixture.py"),
    root,
  ]);
  let log = "";
  fixture.stderr.on("data", (data) => (log += data));
  try {
    const port = await new Promise((resolve, reject) => {
      fixture.stdout.once("data", (data) =>
        resolve(Number(String(data).trim())),
      );
      fixture.once("exit", () => reject(Error(log)));
    });
    const origin = `http://127.0.0.1:${port}`;
    const response = await fetch(origin);
    assert.ok(
      response.headers.get("content-security-policy"),
      "Use the production content policy",
    );
    const state = await readTestState(origin);
    const syncContract = await readFixtureSyncContract(origin);
    const lead = state.threads.find((agent) => agent.name === "Other project");
    const transcript = {
      items: [
        {
          id: "image-message",
          role: "assistant",
          streaming: false,
          text: '![Remote](https://external.invalid/pixel.png)\n\nText <svg><image href="https://external.invalid/unapproved-svg.png" /></svg>\n\n<table background="https://external.invalid/unapproved-background.png"><tr><td>Isolated HTML</td></tr></table>',
        },
      ],
      order: ["image-message"],
      replace: true,
      agent: { ...lead, status: "completed", inFlight: false, turnId: null },
    };
    const page = runnerPage;
    const requests = [];
    page.on("request", (request) => {
      if (request.url().startsWith("https://external.invalid/"))
        requests.push(request.url());
    });
    await page.route("https://external.invalid/**", (route) =>
      route.fulfill({
        contentType: "image/png",
        body: Buffer.from(
          "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aN1sAAAAASUVORK5CYII=",
          "base64",
        ),
      }),
    );
    await stubEntityState(page, state, syncContract);
    await page.route("**/api/sync/pull**", (route) => {
      const url = new URL(route.request().url());
      const scope = url.searchParams.get("scope");
      if (scope !== `transcript:${lead.id}`) return route.fallback();
      return route.fulfill({
        headers: apiSchemaHeaders(),
        json: {
          workspaceId: syncContract.identity.workspaceId,
          ...entityPullFixture(state, {
            scope,
            after: Number(url.searchParams.get("after") || 0),
            transcript,
          }),
        },
      });
    });
    await page.route("**/api/transcript**", (route) =>
      route.fulfill({
        headers: apiSchemaHeaders(),
        json: {
          ...transcript,
        },
      }),
    );
    await page.goto(origin);
    await page.locator(`[data-chat="${lead.id}"]`).click();
    const load = page.getByRole("button", { name: "Load image", exact: true });
    await load.waitFor();
    await page.waitForTimeout(300);
    assert.deepEqual(requests, [], "No external image request before consent");
    await load.click();
    const remoteImage = page.locator('img[alt="Remote"]');
    await remoteImage.scrollIntoViewIfNeeded();
    await page.waitForFunction(() => {
      const image = document.querySelector('img[alt="Remote"]');
      return image?.complete && image.naturalWidth === 1;
    });
    assert.deepEqual(requests, ["https://external.invalid/pixel.png"]);
    await page
      .getByRole("button", { name: "Preview Remote", exact: true })
      .click();
    await page.getByRole("dialog").waitFor();
    await page.locator(".image-viewer img").scrollIntoViewIfNeeded();
    await page.waitForFunction(
      () => document.querySelector(".image-viewer img")?.naturalWidth === 1,
    );
    console.log(
      "PASS: production CSP permits confirmed image loads; Markdown and HTML do not load external images automatically",
    );
  } finally {
    // spawnFixture owns the Python server lifecycle, including timeouts.
  }
});
