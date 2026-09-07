#!/usr/bin/env node
// Production renderer, isolated fixture, and controlled delivery acknowledgements.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";

const repo = dirname(dirname(fileURLToPath(import.meta.url)));
const { chromium } = createRequire(join(repo, "web/package.json"))(
  "playwright-core",
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
  browser = await chromium.launch({
    headless: true,
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  });
  for (const sync of [false, true]) {
    const mode = sync ? "rxdb" : "legacy";
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
    const errors = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.addInitScript(() => {
      window.deliveryStreams = [];
      window.EventSource = class extends EventTarget {
        constructor(url) {
          super();
          this.url = url;
          window.deliveryStreams.push(this);
        }
        close() {
          window.deliveryStreams = window.deliveryStreams.filter(
            (stream) => stream !== this,
          );
        }
      };
      window.publishDelivery = (id, payload) => {
        for (const stream of window.deliveryStreams) {
          const url = new URL(stream.url, location.href);
          if (
            url.pathname === "/api/sync/stream" ||
            url.searchParams.get("id") === id
          )
            stream.onmessage?.({ data: JSON.stringify(payload) });
        }
      };
    });
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
    await page.route("**/api/state", (route) => route.fulfill({ json: state }));
    await page.route("**/api/queue?*", (route) =>
      route.fulfill({ json: { items: [] } }),
    );
    await page.route("**/api/transcript?*", (route) =>
      route.fulfill({
        json: history.get(
          new URL(route.request().url()).searchParams.get("id"),
        ),
      }),
    );
    let transcriptPulls = 0;
    if (!sync) {
      await page.route("**/api/sync/**", (route) =>
        route.fulfill({ status: 404, json: { error: "Unsupported sync" } }),
      );
    } else {
      await page.route("**/api/sync/pull?*", (route) => {
        const url = new URL(route.request().url());
        const scope = url.searchParams.get("scope");
        if (scope !== "state" && !scope.startsWith("transcript:"))
          return route.fallback();
        const id = scope.slice("transcript:".length);
        const seq = scope === "state" ? stateRevision : revisions.get(id);
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
                        scope === "state" ? state : history.get(id),
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
    }
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
        ({ id, payload }) => window.publishDelivery(id, payload),
        { id, payload },
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
            element.scrollHeight - element.scrollTop - element.clientHeight < 4,
        );
    const start = async (text, queue = false) => {
      const count = posts.length;
      await input.fill(text);
      if (queue) await input.press("Tab");
      else await page.locator("#send").click();
      await until(
        () => posts.length === count + 1,
        `${mode}: exactly one POST starts`,
      );
      const sent = posts.at(-1);
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
      assert.equal(
        sent.body.delivery,
        queue || !a.inFlight ? "queue" : "steer",
      );
      return sent;
    };
    const accept = async (sent, status = "accepted") => {
      await sent.route.fulfill({ json: { id: sent.body.id, status } });
      await until(
        async () => (await input.inputValue()) === "",
        `${mode}: accepted draft clears`,
      );
    };
    await page.goto(origin);
    await page.locator(`[data-chat="${a.id}"]`).click();
    await page.locator(`[data-message="${a.id}-history-35"]`).waitFor();
    if (sync)
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
    await until(atBottom, `${mode}: explicit send reveals the newest message`);
    assert.match(await row(firstText).innerText(), /Sending/);
    await page.waitForTimeout(220);
    assert.equal(
      await row(firstText).count(),
      1,
      `${mode}: a delayed POST cannot hide the message`,
    );
    await accept(first);
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
      id: `${a.id}:${first.body.id}`,
      clientMessageId: first.body.id,
      role: "user",
      text: firstText,
    };
    await publish(a.id, [...base, echo]);
    await until(
      async () => (await row(firstText).count()) === 1,
      `${mode}: exact acknowledgement removes only the local duplicate`,
    );
    assert.equal(await page.locator(`[data-message="${echo.id}"]`).count(), 1);

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
    if (sync) {
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

    // A queue row can briefly disappear while an older server dispatches it.
    const queuedText = `${mode} queued message survives dispatch`;
    const queued = await start(queuedText, true);
    await accept(queued, "queued");
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
        (await row(queuedText).count()) === 1 &&
        /Queued/.test(await row(queuedText).innerText()),
      `${mode}: one queued row`,
    );
    await publish(a.id, [...queueBase]);
    await page.waitForTimeout(200);
    assert.equal(
      await row(queuedText).count(),
      1,
      `${mode}: dispatch gap retains local queued receipt`,
    );
    await publish(a.id, [...queueBase, { ...queuedEcho, pending: false }]);
    await until(
      async () =>
        (await row(queuedText).count()) === 1 &&
        (await row(queuedText).locator(".message-delivery-status").count()) ===
          0,
      `${mode}: materialized queue row stays unique`,
    );

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
    // RxDB may retain its unresolved receipt without another network request.
    // The legacy transport must inspect the existing identity through the POST.
    if (!sync)
      await until(
        () => posts.length > beforeUncertainRetry,
        "Legacy retry starts with its retained identity",
      );
    else await page.waitForTimeout(250);
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
      async () => (await page.locator("#send-state").innerText()).trim() === "",
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
    assert.deepEqual(errors, [], `${mode}: no renderer exceptions`);
    await page.screenshot({ path: join(evidence, `${mode}.png`) });
    await context.close();
    console.log(
      `${mode}: delivery, reconciliation, queue, rejection, focus, scroll, and chat isolation pass`,
    );
  }
  console.log(`Evidence: ${evidence}`);
} finally {
  await browser?.close();
  fixture.kill("SIGTERM");
}
