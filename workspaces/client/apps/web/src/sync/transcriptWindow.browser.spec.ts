import type { LegacySyncIdentityResponse } from "./syncBrowserFixture.ts";
import type { components } from "../generated/api";
import type { ResourceChangeEvent, ResourceRef } from "./resourceEvents";
import {
  fixturePort,
  type LegacySyncPullResponse,
  fixtureSubscriptions,
  fixtureResources,
} from "./syncBrowserFixture.ts";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import {
  API_SCHEMA_HASH_HEADER,
  apiSchemaHandshakeSse,
  createFixtureViteServer,
  expect,
  protocol3SseEvent,
  readApiSchemaHash,
  test,
} from "../../tests/playwright.ts";
import { SYNC_TRANSCRIPT_INITIAL_TAIL_ITEMS } from "../generated/sharedValues.ts";

test("a transcript window streams without pulls and older history uses one bounded page", async ({
  browser,
}) => {
  test.setTimeout(60_000);
  const workspaceId = "7".repeat(32);
  const all = Array.from({ length: 120 }, (_, index) => ({
    id: `open:item-${String(index).padStart(3, "0")}`,
    role: "assistant",
    text: `History item ${index}`,
    at: index,
  }));
  const pulls: (string | null)[] = [];
  const pages: Record<string, string>[] = [];
  const streams = new Set<import("node:http").ServerResponse>();
  let revision = 0;
  let transcriptSequence = 1;
  const snapshot = {
    items: all.slice(-SYNC_TRANSCRIPT_INITIAL_TAIL_ITEMS),
    truncated: true,
    nextCursor: all.at(-SYNC_TRANSCRIPT_INITIAL_TAIL_ITEMS)?.id ?? null,
    nextAfterCursor: null,
    historyVersion: "window-fixture",
  } satisfies components["schemas"]["TranscriptPageResponse"];
  const baseline = (
    response: import("node:http").ServerResponse,
    resources: ResourceRef[],
  ) => {
    const refs = resources.filter((ref) => ref.kind === "transcript");
    response.write(
      protocol3SseEvent("resources", {
        protocol: 4,
        workspaceId,
        epoch: "window",
        revision: ++revision,
        reason: "initial",
        resources: refs,
        resourceVersions: refs.map(() => ({
          revision: transcriptSequence,
          cursor: { workspaceId, seq: transcriptSequence },
          replyKind: "snapshot",
          transcriptSnapshot: snapshot,
        })),
      } satisfies ResourceChangeEvent),
    );
  };
  const server = await createFixtureViteServer({
    configFile: false,
    root: fileURLToPath(new URL("../..", import.meta.url)),
    server: { host: "127.0.0.1", port: 0 },
    plugins: [
      {
        name: "transcript-window-fixture",
        configureServer(server) {
          server.middlewares.use(async (request, response, next) => {
            const url = new URL(request.url ?? "", "http://localhost");
            const json = (value: unknown) => {
              response.setHeader("Content-Type", "application/json");
              response.setHeader(API_SCHEMA_HASH_HEADER, readApiSchemaHash());
              response.end(JSON.stringify(value));
            };
            if (url.pathname === "/window-check") {
              response.setHeader("Content-Type", "text/html");
              response.end(
                "<!doctype html><title>History window</title><main></main>",
              );
            } else if (url.pathname === "/api/sync/identity")
              json({
                workspaceId,
              } satisfies LegacySyncIdentityResponse);
            else if (url.pathname === "/api/sync/stream") {
              response.setHeader("Content-Type", "text/event-stream");
              response.setHeader("Cache-Control", "no-cache");
              response.write(
                protocol3SseEvent("session", { sessionId: "window-session" }),
              );
              response.write(apiSchemaHandshakeSse());
              baseline(
                response,
                fixtureResources(url.searchParams.get("resources")),
              );
              streams.add(response);
              response.on("close", () => streams.delete(response));
            } else if (url.pathname === "/api/sync/subscriptions") {
              const chunks = [];
              for await (const chunk of request) chunks.push(chunk);
              const update = fixtureSubscriptions(
                Buffer.concat(chunks).toString(),
              );
              for (const stream of streams) {
                stream.write(
                  protocol3SseEvent("subscriptions", {
                    gen: update.gen,
                  } satisfies components["schemas"]["SyncSubscriptionAck"]),
                );
                baseline(
                  stream,
                  update.add.map((entry) => entry.resource),
                );
              }
              json({
                gen: update.gen,
              } satisfies components["schemas"]["SyncSubscriptionAck"]);
            } else if (url.pathname === "/api/sync/pull") {
              pulls.push(url.searchParams.get("scope"));
              json({
                workspaceId,
                documents: [
                  {
                    id: "transcript:open",
                    payload: JSON.stringify(snapshot),
                    seq: transcriptSequence,
                    _deleted: false,
                  },
                ],
                checkpoint: { seq: transcriptSequence },
              } satisfies LegacySyncPullResponse);
            } else if (url.pathname === "/api/transcript/page") {
              pages.push(Object.fromEntries(url.searchParams));
              const before = all.findIndex(
                (item) => item.id === url.searchParams.get("before"),
              );
              const limit = Number(url.searchParams.get("limit"));
              const start = Math.max(0, before - limit);
              json({
                items: all.slice(start, before),
                truncated: start > 0,
                nextCursor: start ? (all[start]?.id ?? null) : null,
                nextAfterCursor: all[before - 1]?.id || null,
                historyVersion: "window-fixture",
              } satisfies components["schemas"]["TranscriptPageResponse"]);
            } else next();
          });
        },
      },
    ],
  });
  const context = await browser.newContext();
  try {
    await server.listen();
    const page = await context.newPage();
    const errors: string[] = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.goto(`http://127.0.0.1:${fixturePort(server)}/window-check`);
    await page.evaluate((workspaceId) => {
      document.documentElement.dataset.workspaceId = workspaceId;
    }, workspaceId);
    await page.addScriptTag({
      type: "module",
      url: "/src/sync/transcriptWindow.browser.fixture.tsx",
    });
    await page.waitForFunction(() => !!window.syncWindow);
    await expect(page.locator("output")).toHaveAttribute(
      "data-count",
      String(SYNC_TRANSCRIPT_INITIAL_TAIL_ITEMS),
    );
    assert.deepEqual(
      pulls,
      [],
      "a normal initial stream window requires zero pulls",
    );
    await page.locator("#history-scroll").evaluate((root) => {
      root.dispatchEvent(
        new WheelEvent("wheel", { deltaY: -2000, bubbles: true }),
      );
      root.scrollTop = 0;
      root.dispatchEvent(new Event("scroll", { bubbles: true }));
    });
    await expect(page.locator("output")).toHaveAttribute(
      "data-count",
      String(2 * SYNC_TRANSCRIPT_INITIAL_TAIL_ITEMS),
    );
    assert.equal(
      pages.length,
      1,
      "one user action issues exactly one page read",
    );
    assert.equal(Number(pages[0]?.limit), SYNC_TRANSCRIPT_INITIAL_TAIL_ITEMS);
    assert.equal(pages[0]?.before, snapshot.nextCursor);
    assert.deepEqual(
      (await page.locator("output").textContent())?.split(","),
      all.slice(-2 * SYNC_TRANSCRIPT_INITIAL_TAIL_ITEMS).map((item) => item.id),
    );
    assert.deepEqual(pulls, []);
    const last = all.at(-1);
    assert.ok(last);
    last.text = "Recovered oversized transcript";
    transcriptSequence++;
    for (const stream of streams)
      stream.write(
        protocol3SseEvent("resources", {
          protocol: 4,
          workspaceId,
          epoch: "window",
          revision: ++revision,
          reason: "change",
          resources: [
            {
              kind: "transcript",
              agentId: "open",
              tail: SYNC_TRANSCRIPT_INITIAL_TAIL_ITEMS,
            },
          ],
          resourceVersions: [{ revision: transcriptSequence }],
        } satisfies ResourceChangeEvent),
      );
    await expect(
      page.getByText("Recovered oversized transcript", { exact: true }),
    ).toBeVisible();
    assert.deepEqual(
      pulls,
      ["transcript:open"],
      "only the oversized interval permits one recovery pull",
    );
    assert.deepEqual(errors, []);
    await page.evaluate(() => window.syncWindow.historyRoot.unmount());
  } finally {
    await context.close();
    for (const response of streams) response.end();
    await server.close();
  }
});
