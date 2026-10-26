import { required } from "../../src/components/componentTestFixtures.ts";
import { createFixtureViteServer as createServer } from "../playwright.ts";
import type { PostBody } from "../../src/api.ts";
import { fixturePostBody } from "../misc-fixture.ts";
import { fixtureOrigin } from "../misc-fixture.ts";
import type {} from "./radio-browser.fixture.tsx";
import {
  test,
  expect,
  API_SCHEMA_HASH_HEADER,
  readApiSchemaHash,
  apiSchemaHandshakeSse,
} from "../playwright.ts";
import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
test("Radio browser", async ({
  browser: testBrowser,
  page: runnerPage,
  context: runnerContext,
}) => {
  test.setTimeout(180_000);
  const root = resolve(import.meta.dirname, "../../");

  const cacheDir = await mkdtemp(join(tmpdir(), "studio-radio-"));
  const streams = new Set<import("node:http").ServerResponse>();
  const server = await createServer({
    configFile: false,
    root,
    cacheDir,
    server: { host: "127.0.0.1", port: 0, hmr: false },
    plugins: [
      {
        name: "radio-fixture",
        configureServer(s) {
          s.middlewares.use("/api/sync/stream", (req, res) => {
            streams.add(res);
            res.on("close", () => streams.delete(res));
            const resources = JSON.parse(
              new URL(req.url ?? "/", "http://localhost").searchParams.get(
                "resources",
              ) || "[]",
            );
            res.writeHead(200, { "Content-Type": "text/event-stream" });
            res.write(apiSchemaHandshakeSse());
            res.write(
              `event: resources\ndata: ${JSON.stringify({ protocol: 4, workspaceId: "1234567890abcdef1234567890abcdef", epoch: "radio-epoch", revision: 1, reason: "initial", resources, resourceVersions: resources.map((resource: import("../../src/sync/resourceEvents.ts").ResourceRef) => ({ resource, revision: 1 })) })}\n\n`,
            );
          });
          s.middlewares.use("/check", (_req, res) => {
            res.setHeader("Content-Type", "text/html");
            res.end(
              '<html><body><div id="root"></div><script type="module" src="/tests/radio/radio-browser.fixture.tsx"></script></body></html>',
            );
          });
        },
      },
    ],
  });
  try {
    await server.listen();
    const page = runnerPage;
    page.setDefaultTimeout(10000);
    await page.setViewportSize({ width: 1100, height: 850 });
    const errors: string[] = [];
    page.on("pageerror", (e) => errors.push(e.message));
    let mode = "lost";
    let releaseResponse: () => void = () => {
      throw new Error("Missing held response");
    };
    let transcriptReads = 0;
    const requests: PostBody<"/api/peer-teams">[] = [];
    await page.route("**/api/sync/identity", (r) =>
      r.fulfill({
        headers: { [API_SCHEMA_HASH_HEADER]: readApiSchemaHash() },
        json: {
          workspaceId: "1234567890abcdef1234567890abcdef",
          syncProtocol: 2,
        },
      }),
    );
    await page.route(
      "**/api/agent-chat?*",
      (r) => (
        transcriptReads++,
        r.fulfill({
          json: {
            messages: [
              {
                id: "1",
                seq: 1,
                sender: "user",
                text: "Compare these approaches",
                created: 1750000000,
              },
              {
                id: "2",
                seq: 2,
                sender: "a",
                senderName: "Astra",
                text: "First reply. **Keep this visible.**",
                created: 1750000010,
              },
              {
                id: "3",
                seq: 3,
                sender: "b",
                senderName: "Claude",
                text: "Second reply with a different view.",
                created: 1750000020,
              },
            ],
            nextBefore: null,
          },
        })
      ),
    );
    await page.route("**/snapshot", async (r) =>
      r.fulfill({
        json: await page.evaluate(() => window.radioFixture.currentRoom),
      }),
    );
    await page.route("**/api/peer-teams", async (r) => {
      requests.push(
        fixturePostBody<"/api/peer-teams">(r.request().postDataJSON()),
      );
      if (mode === "lost") {
        mode = "ok";
        return r.abort("failed");
      }
      if (mode === "reject")
        return r.fulfill({ status: 409, json: { error: "Revision changed" } });
      if (mode === "held")
        await new Promise<void>((resolve) => {
          releaseResponse = resolve;
        });
      return r.fulfill({ json: { room: { id: "radio:t" } } });
    });
    await page.goto(`${fixtureOrigin(server)}/check`, {
      waitUntil: "domcontentloaded",
      timeout: 30000,
    });
    await page.getByText("Second reply with a different view.").waitFor();
    assert.deepEqual(
      await page
        .locator(".radio-message")
        .evaluateAll((es) => es.map((e) => e.getAttribute("data-sender"))),
      ["user", "a", "b"],
    );
    assert.equal(
      await page.locator(".radio-message time[datetime]").count(),
      3,
    );
    await page
      .getByRole("button", { name: "Options for team Research" })
      .click();
    await page.getByRole("menuitem", { name: "Open shared chat" }).click();
    await page.getByRole("button", { name: "Retry", exact: true }).waitFor();
    assert.equal(await page.evaluate(() => window.radioFixture.opened), null);
    await page.getByRole("button", { name: "Retry", exact: true }).click();
    await page.waitForFunction(() => window.radioFixture.opened === "radio:t");
    assert.equal(requests.length, 2);
    assert.deepEqual(requests[0], requests[1]);
    requests.length = 0;
    mode = "lost";
    const input = page.getByRole("textbox", { name: "Message both agents" });
    await input.fill("Hello both");
    await page.getByRole("button", { name: "Send", exact: true }).click();
    await page.getByRole("button", { name: "Retry", exact: true }).waitFor();
    assert.equal(await input.inputValue(), "Hello both");
    assert.equal(await input.isDisabled(), true);
    await page.reload({ waitUntil: "domcontentloaded", timeout: 30000 });
    await page.getByRole("button", { name: "Retry", exact: true }).click();
    await page.waitForFunction(() => !document.querySelector(".radio-retry"));
    assert.equal(requests.length, 2);
    assert.deepEqual(requests[0], requests[1]);
    assert.ok(requests[0] && "rounds" in requests[0]);
    assert.equal(requests[0].rounds, 1);
    assert.equal(requests[0].target, "both");
    mode = "held";
    await page.evaluate(() => {
      window.radioFixture.holdRefresh = true;
      window.radioFixture.refreshCalls = 0;
    });
    await input.fill("Confirmed send");
    const beforeTranscript = transcriptReads;
    await page.getByRole("button", { name: "Send", exact: true }).click();
    await expect.poll(() => typeof releaseResponse).toBe("function");
    assert.equal(await input.inputValue(), "Confirmed send");
    assert.equal(await input.isDisabled(), true);
    assert.equal(
      await page.evaluate(() => window.radioFixture.refreshCalls),
      0,
    );
    const acceptedPosts = requests.length;
    releaseResponse();
    await expect(input).toBeEnabled({ timeout: 2000 });
    assert.equal(await input.inputValue(), "");
    await expect.poll(() => transcriptReads).toBeGreaterThan(beforeTranscript);
    await page.waitForFunction(
      () => typeof window.radioFixture.rejectRefresh === "function",
    );
    await page.evaluate(() => {
      window.radioFixture.holdRefresh = false;
      window.radioFixture.rejectRefresh(Error("Refresh unavailable"));
    });
    await page.waitForFunction(() =>
      window.radioFixture.notifications?.includes("Refresh unavailable"),
    );
    assert.equal(requests.length, acceptedPosts);
    assert.equal(
      await page.evaluate(() =>
        JSON.parse(
          localStorage.getItem("studio-radio-command:radio-test:radio:t") ??
            "null",
        ),
      ),
      null,
    );
    mode = "ok";
    await input.fill("Discuss tradeoffs");
    await page.evaluate(() => (window.radioFixture.failRefresh = true));
    await page
      .getByRole("button", { name: "Discuss (4 replies)", exact: true })
      .click();
    await expect(input).toBeEnabled();
    await page.waitForFunction(() =>
      window.radioFixture.notifications?.includes("Refresh failed"),
    );
    assert.equal(await input.inputValue(), "");
    const posted = requests.length;
    await page.evaluate(() => (window.radioFixture.failRefresh = false));
    assert.equal(await page.locator(".radio-retry").count(), 0);
    assert.equal(requests.length, posted);
    assert.equal(
      required(
        requests.findLast(
          (
            value,
          ): value is Extract<
            PostBody<"/api/peer-teams">,
            { radio_action: "send" }
          > => "radio_action" in value && value.radio_action === "send",
        ),
      ).rounds,
      2,
    );
    mode = "reject";
    await input.fill("Stale proposal");
    await page.getByRole("button", { name: "Send", exact: true }).click();
    await page.getByRole("button", { name: "Refresh", exact: true }).waitFor();
    const rejected = requests.length;
    await page.getByRole("button", { name: "Refresh", exact: true }).click();
    await page.waitForFunction(() => !document.querySelector(".radio-retry"));
    assert.equal(requests.length, rejected);
    assert.equal(await input.inputValue(), "Stale proposal");
    mode = "ok";
    await page.evaluate(() =>
      window.radioFixture.setRoom({
        ...window.radioFixture.currentRoom,
        radio: {
          ...window.radioFixture.radio(),
          status: "speaking",
          speaker: "a",
        },
      }),
    );
    await page.getByRole("button", { name: "Reply next" }).click();
    await page.waitForFunction(() => !document.querySelector(".radio-retry"));
    assert.equal(
      required(requests.at(-1))?.action === "radio"
        ? required(
            requests.findLast(
              (
                value,
              ): value is Extract<
                PostBody<"/api/peer-teams">,
                { action: "radio" }
              > => value.action === "radio",
            ),
          ).radio_action
        : undefined,
      "pass",
    );
    assert.equal(
      required(
        requests.findLast(
          (
            value,
          ): value is Extract<
            PostBody<"/api/peer-teams">,
            { radio_action: "pass" }
          > => "radio_action" in value && value.radio_action === "pass",
        ),
      ).target,
      "b",
    );
    await page.getByRole("button", { name: "Stop", exact: true }).click();
    await page.waitForFunction(() => !document.querySelector(".radio-retry"));
    assert.equal(
      required(requests.at(-1))?.action === "radio"
        ? required(
            requests.findLast(
              (
                value,
              ): value is Extract<
                PostBody<"/api/peer-teams">,
                { action: "radio" }
              > => value.action === "radio",
            ),
          ).radio_action
        : undefined,
      "stop",
    );
    assert.equal(
      await page.getByRole("button", { name: "Send", exact: true }).isEnabled(),
      true,
    );
    await page.getByRole("button", { name: "Send", exact: true }).click();
    await page.waitForFunction(() => !document.querySelector(".radio-retry"));
    assert.equal(
      required(requests.at(-1))?.action === "radio"
        ? required(
            requests.findLast(
              (
                value,
              ): value is Extract<
                PostBody<"/api/peer-teams">,
                { action: "radio" }
              > => value.action === "radio",
            ),
          ).radio_action
        : undefined,
      "send",
    );
    await input.fill("New instruction");
    await page.evaluate(() =>
      window.radioFixture.setRoom({
        ...window.radioFixture.currentRoom,
        radio: {
          ...window.radioFixture.radio(),
          status: "blocked",
          error: "Reply interrupted. Send a new message.",
          active: null,
          speaker: null,
        },
      }),
    );
    assert.equal(
      await page.getByRole("button", { name: "Send", exact: true }).isEnabled(),
      true,
    );
    await page.evaluate(() =>
      window.radioFixture.setRoom({
        ...window.radioFixture.currentRoom,
        radio: {
          ...window.radioFixture.radio(),
          status: "blocked",
          active: {
            eventId: "e",
            agentId: "a",
            epoch: 0,
            identity: [null, null, 0],
            threadId: null,
            through: 0,
          },
          speaker: "a",
        },
      }),
    );
    assert.equal(
      await page
        .getByRole("button", { name: "Send", exact: true })
        .isDisabled(),
      true,
    );
    assert.equal(
      await page.getByRole("button", { name: "Stop", exact: true }).isEnabled(),
      true,
    );
    await page.evaluate(() =>
      window.radioFixture.setRoom({
        ...window.radioFixture.currentRoom,
        radio: {
          ...window.radioFixture.radio(),
          status: "speaking",
          error: null,
          active: {
            eventId: "e",
            agentId: "a",
            epoch: 0,
            identity: [null, null, 0],
            threadId: null,
            through: 0,
          },
          speaker: "a",
        },
      }),
    );
    await page
      .getByRole("button", { name: "Options for team Research" })
      .click();
    await page.getByRole("menuitem", { name: "Open shared chat" }).click();
    await page.waitForFunction(() => window.radioFixture.opened === "radio:t");
    await page.getByRole("menu").waitFor({ state: "hidden" });
    for (const width of [1100, 375]) {
      await page.setViewportSize({ width, height: 850 });
      assert.equal(
        await page.evaluate(
          () => document.documentElement.scrollWidth <= innerWidth,
        ),
        true,
      );
      assert.equal(
        await page
          .getByRole("button", { name: "Send", exact: true })
          .evaluate(
            (e) =>
              e.getBoundingClientRect().bottom <=
              (document.getElementById("root")?.getBoundingClientRect()
                .bottom ?? 0),
          ),
        true,
      );
      await page.screenshot({
        path: join(tmpdir(), `studio-radio-${width}.png`),
        fullPage: true,
      });
    }
    assert.deepEqual(errors, []);
    console.log(
      `PASS radio UI: ordered transcript, timestamps, durable exact retry, acknowledged refresh, rejected revision, pass, stop, open and responsive layout. Screenshots ${tmpdir()}/studio-radio-{1100,375}.png`,
    );
  } finally {
    await runnerPage.close();
    for (const stream of streams) stream.end();
    await Promise.all(
      testBrowser
        .contexts()
        .filter((ownedContext) => ownedContext !== runnerContext)
        .map((ownedContext) => ownedContext.close()),
    );
    await server.close();
    await rm(cacheDir, { recursive: true, force: true });
  }
});
