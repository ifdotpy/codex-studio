import type { LegacySyncIdentityResponse } from "./syncBrowserFixture.ts";
import type { components } from "../generated/api";
import type { ResourceChangeEvent } from "./resourceEvents";
import { initializeTranscript } from "./transcriptStream.browser.fixture.ts";
import {
  fixturePort,
  legacyPullResponse,
  type FixturePull,
} from "./syncBrowserFixture.ts";
import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { SYNC_STREAM_EVENT_MAX_BYTES as maxBytes } from "../generated/sharedValues";
import {
  API_SCHEMA_HASH_HEADER,
  apiSchemaHandshakeSse,
  expect,
  protocol3SseEvent,
  readApiSchemaHash,
  createFixtureViteServer,
  test,
} from "../../tests/playwright.ts";

test("an open transcript streams text without pulls and recovers incomplete intervals", async ({
  browser,
}) => {
  test.setTimeout(60_000);
  assert.ok(Number.isSafeInteger(maxBytes) && maxBytes > 0);
  const workspaceId = "b".repeat(32);
  const streams = new Set<import("node:http").ServerResponse>();
  const pulls: FixturePull[] = [];
  let sequence = 1;
  let revision = 0;
  let opened = 0;
  let epoch = "transcript-first";
  let text = "Before streaming";
  const item = () => ({ id: "answer", role: "assistant", text });
  const frame = (
    reason: ResourceChangeEvent["reason"],
    after = 0,
    delta?: components["schemas"]["TranscriptStreamDelta"],
  ): ResourceChangeEvent => ({
    protocol: 4,
    workspaceId,
    epoch,
    revision: ++revision,
    reason,
    resources: [{ kind: "transcript", agentId: "open" }],
    resourceVersions: [
      {
        revision: sequence,
        ...(delta
          ? {
              transcriptChanges: { after, through: sequence, delta },
            }
          : {}),
      },
    ],
  });
  const send = (event: ResourceChangeEvent) => {
    for (const response of streams)
      response.write(protocol3SseEvent("resources", event));
  };
  const delta = (): components["schemas"]["TranscriptStreamDelta"] => ({
    delta: true,
    items: [item()],
    removed: [],
    truncated: false,
  });
  const cacheDir = await mkdtemp(
    join(tmpdir(), "studio-transcript-stream-vite-"),
  );
  const server = await createFixtureViteServer({
    configFile: false,
    cacheDir,
    root: fileURLToPath(new URL("../..", import.meta.url)),
    server: { host: "127.0.0.1", port: 0 },
    plugins: [
      {
        name: "transcript-stream-fixture",
        configureServer(server) {
          server.middlewares.use((request, response, next) => {
            const url = new URL(request.url ?? "", "http://localhost");
            if (url.pathname === "/transcript-stream-check") {
              response.setHeader("Content-Type", "text/html");
              response.end(
                "<!doctype html><title>Open transcript</title><main></main>",
              );
              return;
            }
            if (url.pathname === "/api/sync/stream") {
              response.setHeader("Content-Type", "text/event-stream");
              response.setHeader("Cache-Control", "no-cache");
              response.write(apiSchemaHandshakeSse());
              response.write(
                protocol3SseEvent(
                  "resources",
                  frame(opened++ ? "reconnect" : "initial"),
                ),
              );
              streams.add(response);
              response.on("close", () => streams.delete(response));
              return;
            }
            if (
              url.pathname === "/api/sync/identity" ||
              url.pathname === "/api/sync/pull"
            ) {
              response.setHeader("Content-Type", "application/json");
              response.setHeader(API_SCHEMA_HASH_HEADER, readApiSchemaHash());
              if (url.pathname === "/api/sync/identity") {
                response.end(
                  JSON.stringify({
                    workspaceId,
                  } satisfies LegacySyncIdentityResponse),
                );
                return;
              }
              const scope = url.searchParams.get("scope");
              assert.ok(scope);
              const after = Number(url.searchParams.get("after"));
              pulls.push({ scope, after });
              response.end(
                legacyPullResponse({
                  workspaceId,
                  documents: [
                    {
                      id: scope,
                      payload: JSON.stringify({
                        items: [item()],
                        truncated: false,
                      }),
                      seq: sequence,
                      _deleted: false,
                    },
                  ],
                  checkpoint: { seq: sequence },
                }),
              );
              return;
            }
            next();
          });
        },
      },
    ],
  });
  const context = await browser.newContext();
  try {
    await server.listen();
    const page = await context.newPage();
    await page.goto(
      `http://127.0.0.1:${fixturePort(server)}/transcript-stream-check`,
    );
    await page.evaluate(initializeTranscript);
    await expect(page.locator("main")).toHaveText(text);
    const settledPulls = pulls.length;
    text = "Agent streaming text";
    const after = sequence++;
    const event = frame("change", after, delta());
    send(event);
    await expect(page.locator("main")).toHaveText(text);
    await expect
      .poll(() => page.evaluate(() => window.syncTranscript.cursor()))
      .toBe(sequence);
    assert.equal(
      pulls.length,
      settledPulls,
      "streaming text must cause zero transcript pulls after baseline",
    );
    send(event);
    await page.evaluate(() => new Promise(requestAnimationFrame));
    assert.equal(pulls.length, settledPulls, "exact retry is idempotent");
    for (const recovery of ["gap", "epoch", "oversize", "reconnect"]) {
      const before = pulls.length;
      const after = sequence;
      sequence += recovery === "gap" ? 2 : 1;
      text = `Recovered ${recovery}`;
      if (recovery === "epoch") {
        epoch = "transcript-restarted";
        sequence = 1;
      }
      if (recovery === "reconnect") {
        for (const response of streams) response.end();
      } else {
        const payload = delta();
        if (recovery === "oversize") {
          const first = payload.items[0];
          assert.ok(first);
          first.text = "x".repeat(maxBytes + 1);
        }
        send(frame("change", recovery === "gap" ? after + 1 : after, payload));
      }
      await expect(page.locator("main")).toHaveText(text);
      assert.ok(
        pulls.length > before,
        `${recovery} requires a transcript pull`,
      );
    }
    await page.evaluate(() => window.syncTranscript.stopTranscript());
  } finally {
    await context.close();
    for (const response of streams) response.end();
    await server.close();
    await rm(cacheDir, { recursive: true, force: true });
  }
});
