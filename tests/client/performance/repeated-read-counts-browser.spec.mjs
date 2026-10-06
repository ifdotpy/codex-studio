// Count real renderer GETs against the isolated repository fixture backend.
import assert from "node:assert/strict";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "../playwright.mjs";
import { spawnFixture as spawn } from "../playwright.mjs";

const measured = new Set([
  "/api/models",
  "/api/limits",
  "/api/accounts",
  "/api/desktop",
  "/api/costs",
  "/api/worktree-disk",
]);

test("repeated read counts across chat navigation", async ({ browser }) => {
  test.setTimeout(180_000);
  const repo = dirname(
    dirname(dirname(dirname(fileURLToPath(import.meta.url)))),
  );
  const state = await mkdtemp(join(tmpdir(), "repeated-read-counts-"));
  const fixture = spawn(
    "python3",
    ["-B", join(repo, "tests/simple-ui-fixture.py"), state],
    { stdio: ["pipe", "pipe", "pipe"] },
  );
  let log = "";
  fixture.stderr.on("data", (chunk) => (log += chunk));
  const port = await new Promise((resolve, reject) => {
    fixture.stdout.once("data", (chunk) =>
      resolve(Number(String(chunk).trim())),
    );
    fixture.once("exit", () => reject(Error(log)));
  });
  const context = await browser.newContext(); // clean profile for each run
  try {
    const page = await context.newPage();
    const counts = new Map();
    const resourceEvents = [];
    await page.addInitScript(() => {
      const OriginalEventSource = window.EventSource;
      window.EventSource = class extends OriginalEventSource {
        constructor(...args) {
          super(...args);
          this.addEventListener("resources", (event) => {
            window.__resourceEvents.push(JSON.parse(event.data));
          });
        }
      };
      window.__resourceEvents = [];
    });
    page.on("request", (request) => {
      if (request.method() !== "GET") return;
      const url = new URL(request.url());
      if (!measured.has(url.pathname)) return;
      const key = `${url.pathname}${url.search}`;
      counts.set(key, (counts.get(key) || 0) + 1);
    });
    const snapshot = () => Object.fromEntries([...counts].sort());
    const waitForQuiet = async () => {
      let previous = "";
      let stable = 0;
      await page.waitForFunction(() => !!document.querySelector("#message"));
      await page.waitForFunction(() => {
        const text = document.querySelector("#message");
        return !!text && !text.disabled;
      });
      while (stable < 4) {
        await page.waitForTimeout(250);
        const current = JSON.stringify(snapshot());
        stable = current === previous ? stable + 1 : 0;
        previous = current;
      }
    };
    await page.goto(`http://127.0.0.1:${port}`);
    await waitForQuiet();
    const load = snapshot();
    resourceEvents.push(
      ...(await page.evaluate(() => window.__resourceEvents)),
    );

    counts.clear();
    await page
      .locator("[data-chat]")
      .filter({ hasText: "Other project" })
      .click();
    await waitForQuiet();
    const toOther = snapshot();

    counts.clear();
    await page
      .locator("[data-chat]")
      .filter({ hasText: "Release lead" })
      .click();
    await waitForQuiet();
    const back = snapshot();
    console.log(
      `REPEATED_READ_COUNTS ${JSON.stringify({ load, toOther, back, resourceEvents: resourceEvents.map(({ reason, resources }) => ({ reason, resources })) })}`,
    );
    for (const [phase, countsInPhase] of Object.entries({
      load,
      toOther,
      back,
    }))
      for (const [key, count] of Object.entries(countsInPhase))
        assert.ok(
          count <= (phase === "load" && key.startsWith("/api/costs?") ? 2 : 1),
          `${phase} repeated ${key} ${count} times`,
        );
    assert.deepEqual(
      await page
        .locator("[data-chat]")
        .filter({ hasText: "Release lead" })
        .count(),
      1,
    );
  } finally {
    await context.close();
    fixture.kill("SIGTERM");
  }
});
