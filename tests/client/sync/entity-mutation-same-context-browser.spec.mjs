import assert from "node:assert/strict";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnFixture, test } from "../playwright.mjs";

test("mutation projections propagate between pages sharing one browser context @sync", async ({
  browser,
}) => {
  test.setTimeout(90_000);
  const repo = dirname(
    dirname(dirname(dirname(fileURLToPath(import.meta.url)))),
  );
  const state = await mkdtemp(join(tmpdir(), "entity-same-context-"));
  const fixture = spawnFixture(
    "python3",
    ["-B", join(repo, "tests/simple-ui-fixture.py"), state],
    {
      stdio: ["pipe", "pipe", "pipe"],
      env: { ...process.env, RESOURCE_FRESHNESS_UI_FIXTURE: "1" },
    },
  );
  let log = "";
  let output = "";
  fixture.stdout.on("data", (chunk) => {
    output += String(chunk);
  });
  fixture.stderr.on("data", (chunk) => (log += String(chunk)));
  const port = await new Promise((resolve, reject) => {
    fixture.stdout.once("data", (chunk) =>
      resolve(Number(String(chunk).trim())),
    );
    fixture.once("exit", () => reject(new Error(log)));
  });
  const context = await browser.newContext();
  const pages = [await context.newPage(), await context.newPage()];
  const pulls = [[], []];
  const countPulls = [0, 0];
  const waitFor = async (predicate, message, timeoutMs = 10000) => {
    const end = Date.now() + timeoutMs;
    while (Date.now() < end) {
      if (await predicate()) return;
      await new Promise((resolve) => setTimeout(resolve, 40));
    }
    assert.fail(message);
  };
  try {
    for (const [index, page] of pages.entries()) {
      page.setDefaultTimeout(12_000);
      page.on("request", (request) => {
        const url = new URL(request.url());
        if (
          url.pathname === "/api/sync/pull" &&
          url.searchParams.get("scope") === "state:entities:v1"
        ) {
          countPulls[index]++;
          pulls[index].push({ after: url.searchParams.get("after") });
        }
      });
      page.on("response", async (response) => {
        const url = new URL(response.url());
        if (
          url.pathname !== "/api/sync/pull" ||
          url.searchParams.get("scope") !== "state:entities:v1"
        )
          return;
        try {
          const body = await response.json();
          const pull = pulls[index].at(-1);
          if (pull) pull.documents = body.documents || [];
        } catch {}
      });
      await page.addInitScript(() => {
        const OriginalEventSource = window.EventSource;
        window.__entityPersisterCompletions = [];
        window.__studioSyncEntityPersisterProbe = (value) =>
          window.__entityPersisterCompletions.push(value);
        window.__entityFrames = [];
        window.__delayedResourceFrameDelays = [];
        window.__entityStreamUrls = [];
        window.__delayResourceFrames = false;
        const OriginalBroadcastChannel = window.BroadcastChannel;
        window.BroadcastChannel = class extends OriginalBroadcastChannel {
          set onmessage(listener) {
            if (typeof listener !== "function") {
              super.onmessage = listener;
              return;
            }
            super.onmessage = (event) => {
              if (
                window.__delayResourceFrames &&
                event.data?.kind === "resource-event"
              ) {
                const scheduledAt = performance.now();
                setTimeout(() => {
                  window.__delayedResourceFrameDelays.push(
                    performance.now() - scheduledAt,
                  );
                  listener.call(this, event);
                }, 1800);
              } else listener.call(this, event);
            };
          }
        };
        window.EventSource = class extends OriginalEventSource {
          constructor(...args) {
            super(...args);
            window.__entityStreamUrls.push(String(args[0]));
            const streamIndex = window.__entityStreamUrls.length - 1;
            this.addEventListener("resources", (event) =>
              window.__entityFrames.push({
                streamIndex,
                event: JSON.parse(event.data),
              }),
            );
          }
          addEventListener(type, listener, options) {
            if (type !== "resources")
              return super.addEventListener(type, listener, options);
            const delayed = (event) => {
              if (window.__delayResourceFrames) {
                const scheduledAt = performance.now();
                setTimeout(() => {
                  window.__delayedResourceFrameDelays.push(
                    performance.now() - scheduledAt,
                  );
                  listener.call(this, event);
                }, 1800);
              } else listener.call(this, event);
            };
            return super.addEventListener(type, delayed, options);
          }
        };
      });
      await page.goto(`http://127.0.0.1:${port}`);
      await page
        .locator("[data-chat]")
        .filter({ hasText: "Release lead" })
        .click();
    }
    await Promise.all(
      pages.map((page) =>
        page.locator("#conversation-title").getByText("Release lead").waitFor(),
      ),
    );
    await waitFor(
      () =>
        pages.every(
          (_, index) =>
            pulls[index].length === countPulls[index] &&
            pulls[index].every((pull) => pull.documents?.length > 0),
        ),
      "initial state pulls did not settle with rows",
    );
    let streamSnapshots = [];
    await waitFor(async () => {
      streamSnapshots = await Promise.all(
        pages.map((page) =>
          page.evaluate(() => ({
            streamUrls: window.__entityStreamUrls,
            frames: window.__entityFrames.length,
          })),
        ),
      );
      return streamSnapshots.some(
        (snapshot) => snapshot.streamUrls.length > 0 && snapshot.frames > 0,
      );
    }, "no page received the initial resource stream frame");
    const streamOwnerIndex = streamSnapshots.findIndex(
      (snapshot) => snapshot.streamUrls.length > 0 && snapshot.frames > 0,
    );
    assert.notEqual(
      streamOwnerIndex,
      -1,
      "one same-context page owns the resource stream",
    );
    await pages[1].evaluate(() => {
      window.__delayResourceFrames = true;
    });
    await new Promise((resolve) => setTimeout(resolve, 350));

    const createChat = async (page) => {
      await page.locator(".project-tree-heading").first().hover();
      const responsePromise = page.waitForResponse(
        (response) =>
          response.request().method() === "POST" &&
          new URL(response.url()).pathname === "/api/leads",
      );
      await page
        .getByRole("button", { name: /^New chat in / })
        .first()
        .click();
      const response = await responsePromise;
      assert.equal(response.status(), 200);
      const body = await response.json();
      assert.ok(
        body._syncEntities?.length,
        "mutation response includes entity rows",
      );
      await page.waitForFunction(
        (sequences) =>
          window.__entityPersisterCompletions.some((completion) =>
            sequences.every((sequence) =>
              completion.sequences.includes(sequence),
            ),
          ),
        body._syncEntities.map((row) => row.seq),
      );
      const completion = await page.evaluate(
        (sequences) =>
          window.__entityPersisterCompletions.find((candidate) =>
            sequences.every((sequence) =>
              candidate.sequences.includes(sequence),
            ),
          ),
        body._syncEntities.map((row) => row.seq),
      );
      assert.equal(
        completion.advanced,
        true,
        "persister advanced the shared checkpoint",
      );
      await page
        .locator("#conversation-title")
        .getByText("New chat", { exact: true })
        .waitFor();
      return body.id;
    };

    // Pull start is timestamped in Node from Playwright's request event, while
    // persister completion is timestamped in the renderer page; do not compare
    // those clocks. This test checks observed rows and pull counts directly.
    const pullsBeforeFirst = [...countPulls];
    const firstId = await createChat(pages[0]);
    await pages[1].locator(`[data-chat="${firstId}"]`).waitFor();
    await new Promise((resolve) => setTimeout(resolve, 2100));
    assert.ok(
      (await pages[1].evaluate(() => window.__delayedResourceFrameDelays)).some(
        (delay) => delay >= 1750,
      ),
      "observer received its resource frame after the configured delay",
    );
    assert.deepEqual(
      countPulls,
      pullsBeforeFirst,
      "observer uses the shared projection without pulling",
    );

    const pullsBeforeReverse = [...countPulls];
    const secondId = await createChat(pages[1]);
    await pages[0].locator(`[data-chat="${secondId}"]`).waitFor();
    await new Promise((resolve) => setTimeout(resolve, 2100));
    assert.deepEqual(
      countPulls,
      pullsBeforeReverse,
      "reverse mutation also propagates without pulling",
    );
    assert.ok(
      pulls.every((tabPulls) =>
        tabPulls.every((pull) => pull.documents?.length > 0),
      ),
      "every state pull returns rows",
    );
  } finally {
    await context.close();
    fixture.kill("SIGTERM");
  }
});
