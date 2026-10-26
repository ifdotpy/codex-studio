import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import type { ServerResponse } from "node:http";
import { fixtureUrl } from "../usage/fixture-server.ts";
import type {} from "./realtime-voice-ux-ui.fixture.tsx";
import {
  createFixtureViteServer as createServer,
  apiSchemaHeaders,
  apiSchemaHandshakeSse,
  test,
} from "../playwright.ts";

test("Realtime voice ux", async ({ context }, testInfo) => {
  test.setTimeout(180_000);
  // React fixture: no microphone, native account, or model request.
  const root = fileURLToPath(new URL("../..//", import.meta.url));
  const workspaceId = "0123456789abcdef0123456789abcdef";
  const server = await createServer({
    configFile: false,
    root,
    server: { host: "127.0.0.1", port: 0, hmr: false, watch: null },
    plugins: [
      {
        name: "voice-fixture",
        configureServer(s) {
          type ResourceStream = { res: ServerResponse; resources: unknown[] };
          const streams = new Set<ResourceStream>();
          let revision = 0;
          const epoch = "voice-fixture-epoch";
          const writeResourceEvent = (
            stream: ResourceStream,
            reason: string,
          ) => {
            revision++;
            stream.res.write(
              `event: resources\ndata: ${JSON.stringify({ protocol: 4, workspaceId, epoch, revision, reason, resources: stream.resources })}\n\n`,
            );
          };
          s.middlewares.use("/audit/resource-change", (req, res) => {
            if (req.method !== "POST") {
              res.statusCode = 405;
              res.end();
              return;
            }
            for (const stream of streams) writeResourceEvent(stream, "change");
            res.end("ok");
          });
          s.middlewares.use("/audit", (_, res) => {
            res.setHeader("Content-Type", "text/html");
            res.end(
              '<div id="root" style="padding:200px 24px"></div><script type="module" src="/tests/voice/realtime-voice-ux-ui.fixture.tsx"></script>',
            );
          });
          s.middlewares.use("/api/sync/stream", (req, res) => {
            const query = new URL(req.url || "/", "http://localhost")
              .searchParams;
            const stream = {
              res,
              resources: JSON.parse(query.get("resources") || "[]"),
            };
            res.writeHead(200, {
              "Content-Type": "text/event-stream",
              "Cache-Control": "no-cache",
              Connection: "keep-alive",
              ...apiSchemaHeaders(),
            });
            res.write(apiSchemaHandshakeSse());
            streams.add(stream);
            writeResourceEvent(stream, "initial");
            const heartbeat = setInterval(() => {
              res.write(
                `event: heartbeat\ndata: ${JSON.stringify({ protocol: 4, workspaceId, epoch, revision })}\n\n`,
              );
            }, 1000);
            res.on("close", () => {
              clearInterval(heartbeat);
              streams.delete(stream);
            });
          });
        },
      },
    ],
  });
  await server.listen();
  try {
    const page = await context.newPage();
    await page.clock.install();
    await page.setViewportSize({ width: 500, height: 700 });
    const errors: string[] = [];
    page.on("pageerror", (e) => errors.push(e.message));
    async function reset() {
      await page.goto(fixtureUrl(server) + "audit");
      await page
        .getByRole("button", { name: "Voice conversation", exact: true })
        .click();
    }
    async function start() {
      await page.waitForFunction(
        () => window.realtimeVoiceFixture.voiceTransport === "live",
        null,
        {
          timeout: 10000,
        },
      );
      await page
        .getByRole("button", { name: "Start voice", exact: true })
        .click();
      await page.waitForFunction(() =>
        window.realtimeVoiceFixture.calls.some((c) => c[0] === "start"),
      );
    }
    await reset();
    await start();
    await page.waitForFunction(
      () => window.realtimeVoiceFixture.answer?.sdp === "answer",
    );
    assert.deepEqual(
      await page.evaluate(() => window.realtimeVoiceFixture.order),
      ["status", "permission", "capture"],
    );
    assert.equal(
      await page
        .getByRole("button", { name: "Send transcript", exact: true })
        .count(),
      0,
    );
    const readsBeforeTranscriptChange = await page.evaluate(
      () =>
        window.realtimeVoiceFixture.calls.filter(
          (call) => call[0] === "records",
        ).length,
    );
    await page.evaluate(async () => {
      window.realtimeVoiceFixture.emit({
        type: "delegation.created",
        item: { id: "task" },
      });
      window.realtimeVoiceFixture.records = [
        {
          id: "one",
          agent: "chat-one",
          created: 0,
          item_id: "",
          payload: "",
          previous_item_id: "",
          session: window.realtimeVoiceFixture.sid ?? "",
          seq: 1,
          kind: "user",
          text: "Native canonical transcript",
        },
      ];
      await window.realtimeVoiceFixture.resourceChange();
    });
    await page.waitForFunction(
      (count) =>
        window.realtimeVoiceFixture.calls.filter(
          (call) => call[0] === "records",
        ).length ===
        count + 1,
      readsBeforeTranscriptChange,
    );
    await page.waitForFunction(() =>
      [...document.querySelectorAll('[role="status"]')].some(
        (status) => status.textContent === "Working",
      ),
    );
    await page
      .locator(".realtime-voice details summary")
      .evaluate((summary) => {
        if (summary instanceof HTMLElement) summary.click();
      });
    await page.waitForFunction(() =>
      document.body.textContent?.includes("Native canonical transcript"),
    );
    assert.equal(
      await page.evaluate(
        () =>
          window.realtimeVoiceFixture.calls.filter((c) =>
            ["submit", "record", "speech", "approve"].includes(c[0]),
          ).length,
      ),
      0,
    );
    await page.getByRole("button", { name: "Mute", exact: true }).click();
    await page.getByRole("button", { name: "Unmute", exact: true }).waitFor();
    await page.screenshot({
      path: testInfo.outputPath("studio-native-voice-ui.png"),
    });
    await page.getByRole("button", { name: "End voice", exact: true }).click();
    await page.waitForFunction(
      () =>
        window.realtimeVoiceFixture.stops === 1 &&
        window.realtimeVoiceFixture.calls.some((c) => c[0] === "end"),
    );
    await reset();
    await start();
    await page.waitForFunction(() =>
      window.realtimeVoiceFixture.calls.some((call) => call[0] === "records"),
    );
    await page.waitForFunction(
      () => window.realtimeVoiceFixture.voiceTransport === "live",
    );
    const readsBeforeOffline = await page.evaluate(
      () =>
        window.realtimeVoiceFixture.calls.filter(
          (call) => call[0] === "records",
        ).length,
    );
    const statusBeforeOffline = await page.evaluate(
      () =>
        window.realtimeVoiceFixture.calls.filter((call) => call[0] === "status")
          .length,
    );
    await context.setOffline(true);
    await page.waitForFunction(() =>
      [...document.querySelectorAll('[role="alert"]')].some((alert) =>
        alert.textContent?.includes("The microphone is off."),
      ),
    );
    assert.equal(
      await page.evaluate(() => window.realtimeVoiceFixture.stops),
      1,
    );
    assert.equal(
      await page.evaluate(
        () =>
          window.realtimeVoiceFixture.calls.filter(
            (call) => call[0] === "status",
          ).length,
      ),
      statusBeforeOffline,
    );
    assert.equal(
      await page.evaluate(
        () =>
          window.realtimeVoiceFixture.calls.filter(
            (call) => call[0] === "records",
          ).length,
      ),
      readsBeforeOffline,
    );
    await context.setOffline(false);
    await page.waitForFunction(
      (count) =>
        window.realtimeVoiceFixture.calls.filter(
          (call) => call[0] === "records",
        ).length ===
        count + 1,
      readsBeforeOffline,
    );
    await page.waitForFunction(
      () => window.realtimeVoiceFixture.voiceTransport === "live",
    );
    assert.equal(
      await page.evaluate(
        () =>
          window.realtimeVoiceFixture.calls.filter(
            (call) => call[0] === "records",
          ).length,
      ),
      readsBeforeOffline + 1,
      JSON.stringify(
        await page.evaluate(() => window.realtimeVoiceFixture.calls),
      ),
    );
    await reset();
    await page.evaluate(() => (window.realtimeVoiceFixture.pending = true));
    await start();
    assert.equal(
      await page.evaluate(() => window.realtimeVoiceFixture.answer),
      undefined,
    );
    await page.evaluate(async () => {
      window.realtimeVoiceFixture.pending = false;
      await window.realtimeVoiceFixture.resourceChange();
    });
    await page.waitForFunction(
      () => window.realtimeVoiceFixture.answer?.sdp === "answer",
    );
    await reset();
    await page.evaluate(() => {
      window.realtimeVoiceFixture.pending = true;
      window.realtimeVoiceFixture.holdStart = true;
    });
    const recordsBeforeEarlyAnswer = await page.evaluate(
      () =>
        window.realtimeVoiceFixture.calls.filter(
          (call) => call[0] === "records",
        ).length,
    );
    await start();
    await page.evaluate(async () => {
      window.realtimeVoiceFixture.pending = false;
      await window.realtimeVoiceFixture.resourceChange();
    });
    await page.waitForFunction(
      (count) =>
        window.realtimeVoiceFixture.calls.filter(
          (call) => call[0] === "records",
        ).length ===
        count + 1,
      recordsBeforeEarlyAnswer,
    );
    assert.equal(
      await page.evaluate(() => window.realtimeVoiceFixture.answer),
      undefined,
    );
    await page.evaluate(() => {
      window.realtimeVoiceFixture.holdStart = false;
      window.realtimeVoiceFixture.releaseStart?.();
    });
    await page.waitForFunction(
      () => window.realtimeVoiceFixture.answer?.sdp === "answer",
    );
    assert.equal(
      await page.evaluate(() => window.realtimeVoiceFixture.answers),
      1,
      "the buffered records answer is applied once after the stale start response",
    );
    assert.equal(
      await page.evaluate(() => window.realtimeVoiceFixture.stops),
      0,
    );
    await page.evaluate(async () => {
      window.realtimeVoiceFixture.voiceError = "Voice connection failed";
      await window.realtimeVoiceFixture.resourceChange();
    });
    await page.waitForFunction(() =>
      [...document.querySelectorAll('[role="alert"]')].some((alert) =>
        alert.textContent?.includes("Voice connection failed"),
      ),
    );
    assert.equal(
      await page.evaluate(() => window.realtimeVoiceFixture.stops),
      1,
    );
    await reset();
    await page.evaluate(
      () => (window.realtimeVoiceFixture.holdPermission = true),
    );
    await page
      .getByRole("button", { name: "Start voice", exact: true })
      .click();
    await page.waitForFunction(
      () => !!window.realtimeVoiceFixture.releasePermission,
    );
    await page.getByRole("button", { name: "End voice", exact: true }).click();
    await page.evaluate(() =>
      window.realtimeVoiceFixture.releasePermission?.(),
    );
    assert.equal(
      await page.evaluate(() =>
        window.realtimeVoiceFixture.order.includes("capture"),
      ),
      false,
    );
    await reset();
    await page.evaluate(() => (window.realtimeVoiceFixture.holdStart = true));
    await start();
    const sid = await page.evaluate(() => window.realtimeVoiceFixture.sid);
    await page.getByRole("button", { name: "End voice", exact: true }).click();
    await page.evaluate(() => window.realtimeVoiceFixture.releaseStart?.());
    await page.waitForFunction(() =>
      window.realtimeVoiceFixture.calls.some((c) => c[0] === "end"),
    );
    assert.ok(
      (
        await page.evaluate(() =>
          window.realtimeVoiceFixture.calls.filter((c) => c[0] === "end"),
        )
      ).every((c) => c[1].session_id === sid),
    );
    await reset();
    await start();
    await page.evaluate(() => window.realtimeVoiceFixture.render("chat-two"));
    await page.waitForFunction(() => window.realtimeVoiceFixture.stops === 1);
    assert.equal(
      await page.evaluate(
        () =>
          window.realtimeVoiceFixture.calls
            .filter((c) => c[0] === "end")
            .at(-1)?.[1].agent,
      ),
      "chat-one",
    );
    await reset();
    await start();
    await page.getByRole("button", { name: "Close voice panel" }).click();
    await page
      .getByRole("group", { name: "Active voice conversation" })
      .waitFor();
    await page.getByRole("button", { name: "Mute", exact: true }).click();
    assert.equal(
      await page.evaluate(() => window.realtimeVoiceFixture.track.enabled),
      false,
    );
    await page.getByRole("button", { name: "Unmute", exact: true }).click();
    assert.equal(
      await page.evaluate(() => window.realtimeVoiceFixture.track.enabled),
      true,
    );
    await page.evaluate(async () => {
      window.realtimeVoiceFixture.failRecords = true;
      await window.realtimeVoiceFixture.resourceChange();
    });
    await page.waitForFunction(() =>
      [...document.querySelectorAll('[role="alert"]')].some((alert) =>
        alert.textContent?.includes("The microphone is off."),
      ),
    );
    assert.equal(
      await page.evaluate(() => window.realtimeVoiceFixture.stops),
      1,
    );
    await reset();
    await start();
    await page.waitForFunction(
      () => window.realtimeVoiceFixture.answer?.sdp === "answer",
    );
    await page.clock.pauseAt(await page.evaluate(() => Date.now() + 1000));
    await page.evaluate(() => {
      window.realtimeVoiceFixture.disconnect();
    });
    assert.equal(
      await page.evaluate(() => window.realtimeVoiceFixture.track.enabled),
      false,
    );
    assert.equal(
      await page.evaluate(() => window.realtimeVoiceFixture.stops),
      0,
    );
    await page.clock.runFor(11999);
    assert.equal(
      await page.evaluate(() => window.realtimeVoiceFixture.stops),
      0,
    );
    await page.clock.runFor(1);
    await page
      .getByRole("alert")
      .filter({ hasText: "Voice could not reconnect" })
      .waitFor();
    assert.equal(
      await page.evaluate(() => window.realtimeVoiceFixture.stops),
      1,
    );
    await page.clock.resume();
    await reset();
    await page.evaluate(() =>
      localStorage.setItem(
        "voice-delivery:chat-one",
        JSON.stringify({ editedText: "Saved old voice text" }),
      ),
    );
    await page.reload();
    await page
      .getByRole("button", { name: "Voice conversation", exact: true })
      .click();
    assert.equal(
      await page.getByText("Recovered voice draft", { exact: true }).count(),
      0,
    );
    assert.deepEqual(
      await page.evaluate(() =>
        JSON.parse(localStorage.getItem("voice-delivery:chat-one") ?? "null"),
      ),
      { editedText: "Saved old voice text" },
    );
    assert.deepEqual(errors, []);
    console.log(
      "PASS: native voice UI, canonical transcript, account switch, delayed SDP, connection error, stop during permission and start.",
    );
  } finally {
    await server.close();
  }
});
