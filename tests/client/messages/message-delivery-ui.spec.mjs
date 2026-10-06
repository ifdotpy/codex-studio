// Production renderer, isolated fixture, and controlled delivery acknowledgements.
import assert from "node:assert/strict";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import {
  apiSchemaHandshakeEvent,
  spawnFixture as spawn,
  test,
} from "../playwright.mjs";

const browserContextsByTest = new WeakMap();
test.beforeEach(async ({ browser }, testInfo) => {
  browserContextsByTest.set(testInfo, new Set(browser.contexts()));
});
test.afterEach(async ({ browser }, testInfo) => {
  const initialContexts = browserContextsByTest.get(testInfo) ?? new Set();
  await Promise.all(
    browser
      .contexts()
      .filter((context) => !initialContexts.has(context))
      .map((context) => context.close()),
  );
});

test("message delivery ui", async ({ browser: _browser }) => {
  test.setTimeout(120_000);
  const repo = dirname(
    dirname(dirname(dirname(fileURLToPath(import.meta.url)))),
  );
  const evidence = await mkdtemp(join(tmpdir(), "studio-message-delivery-"));
  const fixture = spawn(
    "python3",
    ["-B", join(repo, "tests/simple-ui-fixture.py"), evidence],
    {
      stdio: ["ignore", "pipe", "pipe"],
    },
  );
  let log = "",
    browser;
  fixture.stderr.on("data", (chunk) => {
    log += chunk;
  });
  async function until(check, label) {
    const deadline = Date.now() + 12000;
    while (Date.now() < deadline) {
      if (await check()) return;
      await new Promise((resolve) => setTimeout(resolve, 40));
    }
    throw new Error(label);
  }
  try {
    const port = await new Promise((resolve, reject) => {
      fixture.stdout.once("data", (chunk) =>
        resolve(Number(String(chunk).trim())),
      );
      fixture.once("exit", () => reject(new Error(log)));
    });
    const origin = `http://127.0.0.1:${port}`;
    const original = await (await fetch(`${origin}/api/state`)).json();
    const identity = await (await fetch(`${origin}/api/sync/identity`)).json();
    const state = {
      ...original,
      threads: original.threads.map((agent) => ({
        ...agent,
        status: "running",
        inFlight: true,
        turnId: "delivery-fixture-turn",
      })),
      runtime: { ...original.runtime, requests: [] },
    };
    const a = state.threads.find((agent) => agent.name === "Other project");
    const b = state.threads.find((agent) => agent.name === "Release lead");
    browser = _browser;
    {
      const mode = "rxdb";
      Object.assign(a, {
        status: "running",
        inFlight: true,
        turnId: "delivery-fixture-turn",
      });
      let stateRevision = 100;
      const context = await browser.newContext({
        viewport: { width: 1440, height: 960 },
      });
      const page = await context.newPage();
      page.setDefaultTimeout(12000);
      const mutations = [];
      page.on("request", (request) => {
        // Voice reads and background draft migration are independent of receipt dismissal.
        if (
          !["GET", "HEAD", "OPTIONS"].includes(request.method()) &&
          !["/api/voice/records", "/api/sync/drafts"].includes(
            new URL(request.url()).pathname,
          )
        )
          mutations.push({
            method: request.method(),
            url: new URL(request.url()).pathname,
          });
      });
      const errors = [];
      page.on("pageerror", (error) => errors.push(error.message));
      await page.addInitScript(
        ({ workspaceId, schemaHandshake }) => {
          window.deliveryStreams = [];
          window.EventSource = class extends EventTarget {
            constructor(url) {
              super();
              this.url = url;
              window.deliveryStreams.push(this);
              queueMicrotask(() => {
                this.dispatchEvent(
                  new MessageEvent("api-schema", {
                    data: JSON.stringify(schemaHandshake),
                  }),
                );
              });
            }
            close() {
              window.deliveryStreams = window.deliveryStreams.filter(
                (stream) => stream !== this,
              );
            }
          };
          let generation = Date.now();
          window.publishDelivery = (_id, _revision) => {
            const revision = ++generation;
            for (const stream of window.deliveryStreams) {
              const url = new URL(stream.url, location.href);
              const scope = url.searchParams.get("scope");
              if (scope?.startsWith("transcript:"))
                stream.dispatchEvent(
                  new MessageEvent("changes", {
                    data: JSON.stringify({
                      protocolVersion: 1,
                      scope,
                      workspaceId,
                      documents: [],
                      cursor: 0,
                    }),
                  }),
                );
              if (url.pathname === "/api/sync/stream" && !scope) {
                const resources = JSON.parse(
                  url.searchParams.get("resources") || "[]",
                );
                stream.dispatchEvent(
                  new MessageEvent("resources", {
                    data: JSON.stringify({
                      protocol: 3,
                      workspaceId,
                      epoch: "fixture-epoch",
                      revision,
                      reason: "change",
                      resources,
                    }),
                  }),
                );
              }
            }
          };
        },
        {
          workspaceId: identity.workspaceId,
          schemaHandshake: apiSchemaHandshakeEvent(),
        },
      );
      const history = new Map(
        [a, b].map((agent) => [
          agent.id,
          {
            agent,
            items: Array.from({ length: 36 }, (_, index) => ({
              id: `${agent.id}-history-${index}`,
              role: "assistant",
              text: `Saved paragraph ${index}. ${"Existing conversation text. ".repeat(22)}`,
            })),
            replace: true,
          },
        ]),
      );
      const revisions = new Map([
        [a.id, 100],
        [b.id, 100],
      ]);
      await page.route(/\/api\/state(?:\?.*)?$/, (route) =>
        route.fulfill({ json: state }),
      );
      const queuedItems = new Map();
      await page.route("**/api/queue?*", (route) => {
        const id = new URL(route.request().url()).searchParams.get("agent");
        return route.fulfill({ json: { items: queuedItems.get(id) || [] } });
      });
      await page.route("**/api/transcript?*", (route) =>
        route.fulfill({
          json: history.get(
            new URL(route.request().url()).searchParams.get("id"),
          ),
        }),
      );
      let transcriptPulls = 0;
      await page.route("**/api/sync/pull?*", (route) => {
        const url = new URL(route.request().url());
        const scope = url.searchParams.get("scope");
        const stateScope = scope === "state:chat";
        if (!stateScope && !scope.startsWith("transcript:"))
          return route.fallback();
        const id = scope.slice("transcript:".length);
        const seq = stateScope ? stateRevision : revisions.get(id);
        const after = Number(url.searchParams.get("after") || 0);
        if (scope.startsWith("transcript:")) transcriptPulls++;
        return route.fulfill({
          json: {
            ...identity,
            documents:
              after < seq
                ? [
                    {
                      id: scope,
                      payload: JSON.stringify(
                        stateScope ? state : history.get(id),
                      ),
                      seq,
                      _deleted: false,
                    },
                  ]
                : [],
            checkpoint: { seq: Math.max(after, seq) },
          },
        });
      });
      const posts = [];
      await page.route("**/api/messages", (route) => {
        assert.equal(route.request().method(), "POST");
        posts.push({ route, body: route.request().postDataJSON() });
      });
      const publish = async (id, items) => {
        const payload = {
          ...history.get(id),
          items,
          replace: true,
          order: items.map((item) => item.id),
        };
        history.set(id, payload);
        revisions.set(id, revisions.get(id) + 1);
        await page.evaluate(
          ({ id, revision }) => window.publishDelivery(id, revision),
          { id, revision: revisions.get(id) },
        );
      };
      const row = (text) =>
        page
          .locator("#messages article.message.user")
          .filter({ has: page.locator(".prose.plain", { hasText: text }) });
      const input = page.locator("#message");
      const atBottom = () =>
        page
          .locator("#messages")
          .evaluate(
            (element) =>
              element.scrollHeight - element.scrollTop - element.clientHeight <
              4,
          );
      const start = async (text) => {
        const count = posts.length;
        await input.fill(text);
        await page.locator("#send").click();
        await until(
          () => posts.length === count + 1,
          `${mode}: exactly one POST starts`,
        );
        const sent = posts.at(-1);
        assert.equal(
          await input.inputValue(),
          "",
          `${mode}: draft clears only after the local outbox owns it`,
        );
        await row(text).waitFor();
        assert.equal(
          await row(text).count(),
          1,
          `${mode}: one immediate local message`,
        );
        assert.equal(
          await input.evaluate((element) => document.activeElement === element),
          true,
          `${mode}: submit retains composer focus`,
        );
        assert.equal(sent.body.delivery, "after_tool");
        return sent;
      };
      const accept = async (sent, status = "accepted", expectedDraft = "") => {
        await sent.route.fulfill({ json: { id: sent.body.id, status } });
        await until(
          async () => !(await page.locator("#send-state").textContent()),
          `${mode}: delivery request settles`,
        );
        await until(
          async () => (await input.inputValue()) === expectedDraft,
          `${mode}: acknowledgement preserves the current draft`,
        );
      };
      await page.goto(origin);
      await page.locator(`[data-chat="${a.id}"]`).click();
      await page.locator(`[data-message="${a.id}-history-35"]`).waitFor();
      await until(
        () => transcriptPulls > 0,
        "Real RxDB transcript projection loads",
      );

      // The HTTP response and the replicated history are separate acknowledgements.
      await page.locator("#messages").evaluate((element) => {
        element.scrollTop = 200;
        element.dispatchEvent(new Event("scroll"));
      });
      const firstText = `${mode} message stays visible through acceptance`;
      const first = await start(firstText);
      await until(
        atBottom,
        `${mode}: explicit send reveals the newest message`,
      );
      assert.match(await row(firstText).innerText(), /Sending/);
      await page.waitForTimeout(220);
      assert.equal(
        await row(firstText).count(),
        1,
        `${mode}: a delayed POST cannot hide the message`,
      );
      const optimisticNode = await row(firstText).elementHandle();
      const pendingBox = await row(firstText).boundingBox();
      await input.fill("Next draft typed during delivery");
      await accept(first, "accepted", "Next draft typed during delivery");
      const acceptedBox = await row(firstText).boundingBox();
      assert.ok(
        Math.abs(pendingBox.height - acceptedBox.height) <= 1,
        `${mode}: receipt does not resize the sent message`,
      );
      assert.ok(
        Math.abs(pendingBox.y - acceptedBox.y) <= 1,
        `${mode}: receipt does not move the sent message`,
      );
      await input.fill("");
      const base = history.get(a.id).items;
      await publish(a.id, [...base]);
      await page.waitForTimeout(180);
      assert.equal(
        await row(firstText).count(),
        1,
        `${mode}: stale history cannot hide accepted local content`,
      );
      // Matching text alone must not acknowledge a different user message.
      const unrelated = {
        id: `${mode}-same-text-other-id`,
        role: "user",
        text: firstText,
      };
      await publish(a.id, [...base, unrelated]);
      await until(
        async () => (await row(firstText).count()) === 2,
        `${mode}: matching text does not erase a distinct message`,
      );
      const echo = {
        id: `${a.id}:materialized-${first.body.id}`,
        clientMessageId: first.body.id,
        role: "user",
        text: firstText,
      };
      await publish(a.id, [...base, echo]);
      await until(
        async () => (await row(firstText).count()) === 1,
        `${mode}: exact acknowledgement removes only the local duplicate`,
      );
      assert.equal(
        await page.locator(`[data-message="${echo.id}"]`).count(),
        1,
      );
      assert.equal(
        await optimisticNode.evaluate((node) => node.isConnected),
        true,
        `${mode}: receipt with a different server ID preserves the message DOM node`,
      );

      // The history can acknowledge delivery before the POST response arrives.
      const earlyText = `${mode} history arrives before HTTP acknowledgement`;
      const early = await start(earlyText);
      const earlyBase = history.get(a.id).items;
      const earlyEcho = {
        id: `${a.id}:${early.body.id}`,
        clientMessageId: early.body.id,
        role: "user",
        text: earlyText,
      };
      await publish(a.id, [...earlyBase, earlyEcho]);
      await until(
        async () =>
          (await row(earlyText).locator(".message-delivery-status").count()) ===
          0,
        `${mode}: history replaces pending display before POST returns`,
      );
      await accept(early);
      await publish(a.id, [...earlyBase]);
      await until(
        async () => (await row(earlyText).count()) === 0,
        `${mode}: late HTTP acknowledgement cannot resurrect an observed receipt`,
      );
      {
        await page.reload();
        await page.locator(`[data-chat="${a.id}"]`).click();
        await page.locator(`[data-message="${a.id}-history-35"]`).waitFor();
        assert.equal(
          await row(earlyText).count(),
          0,
          "RxDB retains the observed receipt across reload after late acknowledgement",
        );
      }

      // Subsequent streaming must respect a deliberate move away from the bottom.
      await page.locator("#messages").evaluate((element) => {
        element.scrollTop = 300;
        element.dispatchEvent(new Event("scroll"));
      });
      const top = await page
        .locator("#messages")
        .evaluate((element) => element.scrollTop);
      await publish(a.id, [
        ...history.get(a.id).items,
        {
          id: `${mode}-stream`,
          role: "assistant",
          text: "New streamed content. ".repeat(80),
          streaming: true,
        },
      ]);
      await page.waitForTimeout(220);
      assert.ok(
        Math.abs(
          (await page
            .locator("#messages")
            .evaluate((element) => element.scrollTop)) - top,
        ) < 3,
        `${mode}: streaming preserves manual scroll`,
      );

      // A delayed projection cannot remove a receipt before materialization.
      const queuedText = `${mode} queued message survives dispatch`;
      const queued = await start(queuedText);
      const queueRow = () =>
        page
          .getByTestId("message-queue")
          .getByText(queuedText, { exact: true });
      const assertQueueGeometry = async () => {
        // After-tool input waits in the chat, never in the after-turn queue.
        await row(queuedText).waitFor();
        assert.equal(await queueRow().count(), 0);
        assert.ok(
          await page.evaluate(
            () => document.documentElement.scrollWidth <= innerWidth + 1,
          ),
        );
      };
      await accept(queued, "queued");
      await assertQueueGeometry("queued receipt");
      await until(
        async () =>
          (await row(queuedText).innerText()).includes("Waiting for agent"),
        "Server acceptance must stop showing an active network send",
      );
      const queueBase = history.get(a.id).items;
      const queuedEcho = {
        id: `${a.id}:${queued.body.id}`,
        clientMessageId: queued.body.id,
        role: "user",
        text: queuedText,
        pending: true,
      };
      await publish(a.id, [...queueBase, queuedEcho]);
      await until(
        async () =>
          (await queueRow().count()) === 0 &&
          (await row(queuedText).count()) === 1,
        `${mode}: one waiting row in the chat`,
      );
      await assertQueueGeometry("queued transcript");
      await page.screenshot({
        path: join(evidence, `${mode}-queued-desktop.png`),
      });
      await page.setViewportSize({ width: 390, height: 844 });
      await row(queuedText).scrollIntoViewIfNeeded();
      await page.waitForTimeout(100);

      assert.ok(
        await page.evaluate(
          () => document.documentElement.scrollWidth <= innerWidth + 1,
        ),
        `${mode}: queued metadata fits the mobile viewport`,
      );
      await page.screenshot({
        path: join(evidence, `${mode}-queued-mobile.png`),
      });
      await publish(a.id, [...queueBase]);
      await page.waitForTimeout(200);
      assert.equal(
        await row(queuedText).count(),
        1,
        `${mode}: dispatch gap retains the local waiting message`,
      );
      assert.equal(await queueRow().count(), 0);
      await publish(a.id, [...queueBase, { ...queuedEcho, pending: false }]);
      await until(
        async () =>
          (await row(queuedText).count()) === 1 &&
          (await row(queuedText)
            .locator(".message-delivery-status")
            .count()) === 0,
        `${mode}: materialized queue row stays unique`,
      );
      assert.equal(await queueRow().count(), 0);
      await page.setViewportSize({ width: 1440, height: 960 });

      // A rejected message keeps its draft, attachments, and an inline error.
      await page.locator('input[type="file"]').setInputFiles({
        name: "delivery-evidence.txt",
        mimeType: "text/plain",
        buffer: Buffer.from("Retain this attachment after a rejected send.\n"),
      });
      const attachment = page.getByRole("button", {
        name: "Remove delivery-evidence.txt",
        exact: true,
      });
      await attachment.waitFor();
      const failedText = `${mode} rejected message remains editable`;
      const failed = await start(failedText);
      assert.equal(failed.body.assets.length, 1);
      await failed.route.fulfill({
        status: 400,
        json: { error: "Fixture rejects this message" },
      });
      await row(failedText).locator(".message-delivery-error").waitFor();
      assert.match(
        await row(failedText).innerText(),
        /Fixture rejects this message/,
      );
      assert.equal(await input.inputValue(), failedText);
      await attachment.waitFor();
      const beforeRejectedRetry = posts.length;
      await page.locator("#send").click();
      await until(
        () => posts.length === beforeRejectedRetry + 1,
        `${mode}: a definite rejection permits another attempt`,
      );
      const rejectedRetry = posts.at(-1);
      assert.notEqual(
        rejectedRetry.body.id,
        failed.body.id,
        `${mode}: retry after HTTP 400 has a fresh request identity`,
      );
      assert.equal(rejectedRetry.body.text, failed.body.text);
      assert.deepEqual(
        rejectedRetry.body.assets,
        failed.body.assets,
        `${mode}: retry retains the same attachment`,
      );
      await accept(rejectedRetry);
      await attachment.waitFor({ state: "hidden" });

      // Unknown delivery does not become permission to submit a fresh message.
      const uncertainText = `${mode} uncertain request retains its identity after reload`;
      const uncertain = await start(uncertainText);
      await uncertain.route.fulfill({
        json: {
          id: uncertain.body.id,
          status: "uncertain",
          error: "Fixture cannot confirm delivery",
        },
      });
      await row(uncertainText).locator(".message-delivery-error").waitFor();
      assert.equal(await input.inputValue(), uncertainText);
      // A timed-out steer can outlive its target turn. The automatic fallback to
      // queue must not assign a new identity to the same unresolved message.
      Object.assign(a, { status: "completed", inFlight: false, turnId: null });
      stateRevision++;
      revisions.set(a.id, revisions.get(a.id) + 1);
      await page.reload();
      await page.locator(`[data-chat="${a.id}"]`).click();
      await until(
        async () => (await input.inputValue()) === uncertainText,
        `${mode}: uncertain draft survives reload`,
      );
      const beforeUncertainRetry = posts.length;
      await page.locator("#send").click();
      // RxDB retains its unresolved receipt without another network request.
      await page.waitForTimeout(250);
      const repeated = posts.slice(beforeUncertainRetry);
      assert.ok(repeated.length <= 1, `${mode}: no concurrent retries`);
      for (const attempt of repeated) {
        assert.equal(
          attempt.body.id,
          uncertain.body.id,
          `${mode}: uncertain reload cannot create a duplicate request identity`,
        );
        assert.deepEqual(attempt.body, uncertain.body);
        await attempt.route.fulfill({
          json: { id: attempt.body.id, status: "accepted" },
        });
      }
      await until(
        async () =>
          (await page.locator("#send-state").innerText()).trim() === "",
        `${mode}: uncertainty handling returns control to the composer`,
      );
      const expectedPosts = posts.length + 1;

      // Completion in A cannot clear B's draft or move B's keyboard focus.
      const switchText = `${mode} pending message belongs to first chat`;
      const switched = await start(switchText);
      await page.locator(`[data-chat="${b.id}"]`).click();
      await page.locator(`[data-message="${b.id}-history-35"]`).waitFor();
      const otherDraft = `${mode} draft in second chat`;
      await input.fill(otherDraft);
      await switched.route.fulfill({
        json: { id: switched.body.id, status: "accepted" },
      });
      await page.waitForTimeout(250);
      assert.equal(
        await input.inputValue(),
        otherDraft,
        `${mode}: old send preserves new chat draft`,
      );
      assert.equal(
        await input.evaluate((element) => document.activeElement === element),
        true,
        `${mode}: old send preserves new chat focus`,
      );
      assert.equal(
        await row(switchText).count(),
        0,
        `${mode}: outgoing message stays scoped to its chat`,
      );
      await page.locator(`[data-chat="${a.id}"]`).click();
      await row(switchText).waitFor();
      assert.equal(
        await row(switchText).count(),
        1,
        `${mode}: returning restores the accepted message`,
      );
      assert.equal(
        posts.length,
        expectedPosts,
        `${mode}: no automatic duplicate delivery`,
      );
      // A waiting after-tool receipt stays in the chat until native dispatch,
      // also after a reload. It never enters the after-turn queue.
      const staleText = `${mode} stale queue receipt`;
      const stale = await start(staleText);
      await accept(stale, "queued");
      await row(staleText).waitFor();
      await page.reload();
      await page.locator(`[data-chat="${a.id}"]`).click();
      await page.locator(`[data-message="${a.id}-history-35"]`).waitFor();
      await row(staleText).waitFor();
      assert.equal(
        await page.getByTestId("message-queue").getByText(staleText).count(),
        0,
        `${mode}: waiting receipt has one visible location`,
      );
      assert.equal(await row(staleText).count(), 1);
      await publish(a.id, [
        ...history.get(a.id).items,
        {
          id: `${a.id}:${stale.body.id}`,
          clientMessageId: stale.body.id,
          role: "user",
          text: staleText,
        },
      ]);
      await row(staleText).waitFor();
      // Suppress stream notifications after the server materializes the input.
      // The existing queue poll must refresh this chat without a page reload.
      const silentText = `${mode} delivered input with a silent stream`;
      const silent = await start(silentText);
      queuedItems.set(a.id, [
        {
          id: silent.body.id,
          kind: "user",
          text: silentText,
          status: "pending",
          delivery: "after_tool",
        },
      ]);
      await accept(silent, "queued");
      await until(
        async () =>
          (await row(silentText).innerText()).includes("Waiting for agent"),
        "Accepted input remains visible while the agent waits",
      );
      const silentBase = history.get(a.id);
      history.set(a.id, {
        ...silentBase,
        items: [
          ...silentBase.items,
          {
            id: `${a.id}:${silent.body.id}`,
            clientMessageId: silent.body.id,
            role: "user",
            text: silentText,
            deliveryStatus: "delivered",
            materialized: true,
            pending: false,
          },
        ],
      });
      revisions.set(a.id, revisions.get(a.id) + 1);
      queuedItems.set(a.id, []);
      const postsBeforeReconcile = posts.length;
      await until(
        async () =>
          (await row(silentText).count()) === 1 &&
          (await row(silentText)
            .locator(".message-delivery-status")
            .count()) === 0,
        "A silent stream cannot leave a delivered input waiting forever",
      );
      assert.equal(
        posts.length,
        postsBeforeReconcile,
        "Receipt reconciliation never resends",
      );

      // Dismissal hides one receipt on this device without cancelling delivery.
      const dismissedText = `${mode} uncertain message removed from this device`;
      const dismissed = await start(dismissedText);
      await dismissed.route.fulfill({
        json: {
          id: dismissed.body.id,
          status: "uncertain",
          error: "Fixture cannot confirm delivery",
        },
      });
      await row(dismissedText).locator(".message-delivery-error").waitFor();
      await until(
        async () =>
          (await page.locator("#send-state").innerText()).trim() === "",
        `${mode}: uncertain response returns control before dismissal`,
      );
      await page.waitForTimeout(350);
      const mutationsBeforeRemoval = mutations.length;
      const remove = row(dismissedText).getByRole("button", {
        name: "Remove message",
        exact: true,
      });
      assert.equal(
        await remove.getAttribute("title"),
        "Remove from this device. Delivery is not cancelled.",
      );
      await page.screenshot({
        path: join(evidence, `${mode}-remove-message.png`),
      });
      await remove.click();
      await row(dismissedText).waitFor({ state: "hidden" });
      assert.equal(
        await input.inputValue(),
        dismissedText,
        `${mode}: dismissal preserves the draft`,
      );
      await page.locator(`[data-chat="${b.id}"]`).click();
      await page.locator(`[data-message="${b.id}-history-35"]`).waitFor();
      await page.locator(`[data-chat="${a.id}"]`).click();
      await page.locator(`[data-message="${a.id}-history-35"]`).waitFor();
      assert.equal(
        await row(dismissedText).count(),
        0,
        `${mode}: dismissal survives a chat switch`,
      );
      await page.reload();
      await page.locator(`[data-chat="${a.id}"]`).click();
      await page.locator(`[data-message="${a.id}-history-35"]`).waitFor();
      assert.equal(
        await row(dismissedText).count(),
        0,
        `${mode}: dismissal survives reload`,
      );
      const dismissedEcho = {
        id: `${a.id}:${dismissed.body.id}`,
        clientMessageId: dismissed.body.id,
        role: "user",
        text: dismissedText,
      };
      const distinctEcho = {
        ...dismissedEcho,
        id: `${mode}-distinct-dismissed-text`,
        clientMessageId: `${mode}-distinct-client-id`,
      };
      await publish(a.id, [
        ...history.get(a.id).items,
        dismissedEcho,
        distinctEcho,
      ]);
      await page.locator(`[data-message="${distinctEcho.id}"]`).waitFor();
      assert.equal(
        await page.locator(`[data-message="${dismissedEcho.id}"]`).count(),
        0,
        `${mode}: late delivery cannot restore a dismissed receipt`,
      );
      assert.equal(
        await row(dismissedText).count(),
        1,
        `${mode}: a different identity with identical text remains visible`,
      );
      assert.equal(
        await row(dismissedText)
          .getByRole("button", { name: "Remove message", exact: true })
          .count(),
        0,
        `${mode}: delivered messages do not expose dismissal`,
      );

      // Server-side failures use the same local dismissal as uncertain local sends.
      const serverBase = [...history.get(a.id).items];
      const removable = ["uncertain", "failed", "cancelled"].map((status) => ({
        id: `${mode}-server-${status}`,
        role: "user",
        text: `${mode} server ${status} receipt`,
        deliveryStatus: status,
        deliveryError: `Fixture ${status} receipt`,
      }));
      const retained = ["accepted", "queued"].map((status) => ({
        id: `${mode}-server-${status}`,
        role: "user",
        text: `${mode} server ${status} receipt`,
        deliveryStatus: status,
      }));
      await publish(a.id, [...serverBase, ...removable, ...retained]);
      for (const message of removable) {
        const item = page.locator(`[data-message="${message.id}"]`);
        await item.waitFor();
        await item
          .getByRole("button", { name: "Remove message", exact: true })
          .click();
        await item.waitFor({ state: "hidden" });
      }
      for (const message of retained)
        assert.equal(
          await page
            .locator(`[data-message="${message.id}"]`)
            .getByRole("button", { name: "Remove message", exact: true })
            .count(),
          0,
          `${mode}: ${message.deliveryStatus} messages do not expose dismissal`,
        );
      await publish(a.id, [
        ...serverBase,
        ...removable.map(
          ({
            deliveryStatus: _deliveryStatus,
            deliveryError: _deliveryError,
            ...message
          }) => message,
        ),
        ...retained,
      ]);
      await page.reload();
      await page.locator(`[data-chat="${a.id}"]`).click();
      await page.locator(`[data-message="${retained.at(-1).id}"]`).waitFor();
      for (const message of [...removable, dismissedEcho])
        assert.equal(
          await page.locator(`[data-message="${message.id}"]`).count(),
          0,
          `${mode}: delivered receipt ${message.id} remains dismissed after reload`,
        );
      assert.equal(
        await page.locator(`[data-message="${distinctEcho.id}"]`).count(),
        1,
      );
      // Message IDs do not share dismissal state across conversations.
      const otherChatReceipt = {
        ...removable[0],
        text: `${mode} same ID in another chat`,
      };
      const refreshedOtherChat =
        mode === "rxdb"
          ? page.waitForResponse(async (response) => {
              const url = new URL(response.url());
              return (
                url.pathname === "/api/sync/pull" &&
                url.searchParams.get("scope") === `transcript:${b.id}` &&
                (await response.text()).includes(otherChatReceipt.id)
              );
            })
          : Promise.resolve();
      await publish(b.id, [...history.get(b.id).items, otherChatReceipt]);
      await page.locator(`[data-chat="${b.id}"]`).click();
      await refreshedOtherChat;
      await row(otherChatReceipt.text).waitFor();
      assert.equal(
        await row(otherChatReceipt.text).count(),
        1,
        `${mode}: dismissal stays scoped to its chat`,
      );
      await page.locator(`[data-chat="${a.id}"]`).click();
      await page.locator(`[data-message="${distinctEcho.id}"]`).waitFor();
      assert.equal(
        await page.locator(`[data-message="${otherChatReceipt.id}"]`).count(),
        0,
      );
      assert.deepEqual(
        mutations.slice(mutationsBeforeRemoval),
        [],
        `${mode}: dismissal does not mutate server state or cancel delivery`,
      );
      assert.deepEqual(errors, [], `${mode}: no renderer exceptions`);
      await page.screenshot({ path: join(evidence, `${mode}.png`) });
      await context.close();
      console.log(
        `${mode}: delivery, reconciliation, queue, rejection, local dismissal, focus, scroll, and chat isolation pass`,
      );
    }
    console.log(`Evidence: ${evidence}`);
  } finally {
    fixture.stdin?.end();
  }
});
