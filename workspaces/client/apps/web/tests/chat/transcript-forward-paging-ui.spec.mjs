import {
  API_SCHEMA_HASH_HEADER,
  entityPullFixtureForRequest,
  readApiSchemaHash,
  readFixtureSyncContract,
  readTestState,
  test,
  spawnFixture,
} from "../playwright.mjs";
import assert from "node:assert/strict";
import { mkdtemp } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { fileURLToPath } from "node:url";

test("transcript paging returns from the oldest row to the newest", async ({
  page,
}) => {
  test.setTimeout(120_000);
  const repo = fileURLToPath(new URL("../../../../../../", import.meta.url));
  const root = await mkdtemp(join(tmpdir(), "studio-transcript-forward-"));
  const fixture = spawnFixture(
    "python3",
    [
      join(repo, "workspaces/runtime/apps/server/src/codex_python.py"),
      "--exec",
      "-B",
      join(repo, "workspaces/runtime/apps/server/tests/simple-ui-fixture.py"),
      root,
    ],
    { stdio: ["ignore", "pipe", "pipe"] },
  );
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
    const state = await readTestState(origin);
    const syncContract = await readFixtureSyncContract(origin);
    const lead = state.threads.find((agent) => agent.name === "Release lead");
    const other = state.threads.find((agent) => agent.name === "Other project");
    assert.ok(lead && other, "fixture has a managed lead chat");

    const items = Array.from({ length: 600 }, (_, index) => ({
      id: `message-${index}`,
      role: index % 2 ? "assistant" : "user",
      at: index + 1,
      turnId: `turn-${Math.floor(index / 2)}`,
      turnStatus: "completed",
      text: `Transcript message ${index}`,
    }));
    const schemaHeaders = {
      [API_SCHEMA_HASH_HEADER]: readApiSchemaHash(),
    };
    const transcriptSyncPulls = [];
    const transcriptRestReads = [];
    const pageRequests = [];
    page.on("request", (request) => {
      if (new URL(request.url()).pathname === "/api/transcript")
        transcriptRestReads.push(request.url());
    });
    await page.addInitScript(({ key, id }) => localStorage.setItem(key, id), {
      key: `codex-desktop-opened:${state.stateDir}`,
      id: JSON.stringify(other.id),
    });
    const initialTranscript = {
      agent: { ...lead, status: "completed", inFlight: false, turnId: null },
      items: items.slice(-240),
      truncated: true,
      nextCursor: items.at(-240).id,
      nextAfterCursor: null,
      historyVersion: "forward-paging-fixture",
      tail: items.at(-1).id,
    };
    await page.route("**/api/sync/pull**", async (route) => {
      const url = new URL(route.request().url());
      const scope = url.searchParams.get("scope");
      if (scope !== `transcript:${lead.id}`) return route.continue();
      transcriptSyncPulls.push({
        scope,
        after: url.searchParams.get("after") || "0",
      });
      const projection = entityPullFixtureForRequest(state, url, {
        transcript: initialTranscript,
      });
      await route.fulfill({
        headers: schemaHeaders,
        json: {
          workspaceId: syncContract.identity.workspaceId,
          ...projection,
        },
      });
    });
    await page.route("**/api/transcript/page?*", async (route) => {
      const url = new URL(route.request().url());
      pageRequests.push({
        id: url.searchParams.get("id"),
        before: url.searchParams.get("before"),
        after: url.searchParams.get("after"),
      });
      const before = url.searchParams.get("before");
      const after = url.searchParams.get("after");
      const boundary = before || after;
      const index = items.findIndex((item) => item.id === boundary);
      const selected = before
        ? items.slice(Math.max(0, index - 120), index)
        : items.slice(index + 1, index + 121);
      const first = items.findIndex((item) => item.id === selected[0]?.id);
      const last = items.findIndex((item) => item.id === selected.at(-1)?.id);
      await route.fulfill({
        headers: schemaHeaders,
        json: {
          items: selected,
          nextCursor: before && first > 0 ? items[first].id : null,
          nextAfterCursor:
            last >= 0 && last < items.length - 1 ? items[last].id : null,
          historyVersion: "forward-paging-fixture",
        },
      });
    });

    await page.goto(origin);
    await page.locator(`[data-chat="${lead.id}"]`).click();
    await page.locator('[data-message="message-599"]').waitFor();

    const earlier = page.locator("#earlier-messages");
    for (let attempt = 0; (await earlier.count()) && attempt < 10; attempt++) {
      await page.locator("#messages").evaluate((root) => {
        root.scrollTop = 0;
        root.dispatchEvent(new Event("scroll"));
      });
      const response = page.waitForResponse(
        (value) =>
          value.url().includes("/api/transcript/page?") &&
          new URL(value.url()).searchParams.has("before"),
      );
      await earlier.click();
      await response;
    }
    assert.equal(
      await earlier.count(),
      0,
      "Earlier messages ends at the oldest row",
    );
    await page.locator('[data-message="message-0"]').waitFor();

    const later = page.locator("#newer-messages");
    let forwardPages = 0;
    for (let attempt = 0; (await later.count()) && attempt < 10; attempt++) {
      await later.scrollIntoViewIfNeeded();
      await page.evaluate(
        () =>
          new Promise((resolve) =>
            requestAnimationFrame(() => requestAnimationFrame(resolve)),
          ),
      );
      const anchor = await page.locator("#messages").evaluate((root) => {
        const bounds = root.getBoundingClientRect();
        const visible = Array.from(
          root.querySelectorAll("[data-message]"),
        ).filter((row) => {
          const box = row.getBoundingClientRect();
          return (
            box.height > 0 && box.bottom > bounds.top && box.top < bounds.bottom
          );
        });
        const row = visible.at(-1);
        if (!row) throw new Error("no transcript row is visible before paging");
        return {
          id: row.dataset.message,
          offset: row.getBoundingClientRect().top - bounds.top,
        };
      });
      const response = page.waitForResponse(
        (value) =>
          value.url().includes("/api/transcript/page?") &&
          new URL(value.url()).searchParams.has("after"),
      );
      await later.click();
      const pageResponse = await response;
      assert.equal(
        new URL(pageResponse.url()).searchParams.get("after"),
        `message-${239 + forwardPages * 120}`,
        "each forward request starts at the held window's newest message",
      );
      await page.evaluate(
        () => new Promise((resolve) => requestAnimationFrame(resolve)),
      );
      await page.waitForFunction(
        ({ id, offset }) => {
          const root = document.querySelector("#messages");
          const row = Array.from(
            root?.querySelectorAll("[data-message]") || [],
          ).find((candidate) => candidate.dataset.message === id);
          if (!root || !row) return false;
          const bounds = root.getBoundingClientRect();
          const box = row.getBoundingClientRect();
          const top = box.top - bounds.top;
          return (
            box.bottom >= bounds.top - 32 &&
            box.top < bounds.bottom &&
            Math.abs(top - offset) <= 32
          );
        },
        anchor,
        { timeout: 10_000 },
      );
      forwardPages++;
    }
    await later.waitFor({ state: "detached" });
    await page.locator("#messages").evaluate((root) => {
      root.scrollTop = root.scrollHeight;
      root.dispatchEvent(new Event("scroll"));
    });
    await page.evaluate(
      () =>
        new Promise((resolve) =>
          requestAnimationFrame(() => requestAnimationFrame(resolve)),
        ),
    );
    await page.locator('[data-message="message-599"]').waitFor();
    assert.equal(
      await page.locator('[data-message="message-599"]').count(),
      1,
      "newest transcript row is rendered again",
    );
    assert.equal(
      await later.count(),
      0,
      "Later messages ends at the newest row",
    );
    assert.equal(forwardPages, 3, "the held window advances through all pages");
    assert.ok(
      transcriptSyncPulls.some(
        (pull) => pull.scope === `transcript:${lead.id}` && pull.after === "0",
      ),
      "the initial transcript arrives through the active transcript sync pull",
    );
    assert.equal(
      transcriptRestReads.length,
      0,
      "the initial transcript does not fall back to REST",
    );
    assert.equal(
      pageRequests.length,
      pageRequests.filter((request) => request.before !== null).length + 3,
      "all history pages use the page API",
    );
    assert.equal(
      pageRequests.filter((request) => request.before !== null).length >= 3,
      true,
      "older pages use before= requests through the oldest row",
    );
    assert.equal(
      pageRequests.filter((request) => request.after !== null).length,
      3,
      "newer pages use after= requests",
    );
  } finally {
    fixture.kill("SIGTERM");
  }
});
