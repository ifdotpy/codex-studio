#!/usr/bin/env node
// Production renderer with controlled transcript timing and isolated server state.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
const repo = dirname(dirname(fileURLToPath(import.meta.url)));
const { chromium } = createRequire(join(repo, "web/package.json"))(
  "playwright-core",
);
const dir = await mkdtemp(join(tmpdir(), "studio-conversation-motion-"));
const proc = spawn(
  "python3",
  ["-B", join(repo, "tests/simple-ui-fixture.py"), dir],
  { stdio: ["ignore", "pipe", "pipe"] },
);
let log = "",
  browser;
proc.stderr.on("data", (d) => (log += d));
const afterPaint = (page, selector) =>
  page.evaluate(
    (rootSelector) =>
      new Promise((resolve) => {
        let previous = "";
        let stableFrames = 0;
        const sample = () => {
          const root = document.querySelector(rootSelector);
          if (!root)
            throw new Error(`Transcript root missing: ${rootSelector}`);
          const composer = root
            .closest("#conversation")
            ?.querySelector("#message");
          const values = [
            root?.scrollTop,
            root?.scrollHeight,
            root?.clientHeight,
            composer?.getBoundingClientRect().height,
          ].join(":");
          stableFrames = values === previous ? stableFrames + 1 : 0;
          previous = values;
          if (stableFrames === 3) resolve();
          else requestAnimationFrame(sample);
        };
        requestAnimationFrame(sample);
      }),
    selector,
  );
try {
  const port = await new Promise((resolve, reject) => {
    proc.stdout.once("data", (d) => resolve(Number(String(d).trim())));
    proc.once("exit", () => reject(Error(log)));
  });
  const origin = `http://127.0.0.1:${port}`;
  const state = await (await fetch(origin + "/api/state")).json();
  const syncIdentity = await (
    await fetch(origin + "/api/sync/identity")
  ).json();
  const lead = state.threads.find((a) => a.name === "Other project");
  const other = state.threads.find((a) => a.name === "Release lead");
  let items = Array.from({ length: 36 }, (_, i) => ({
    id: `note${i}`,
    role: "assistant",
    text: `Observation ${i}.\n\n${"A retained paragraph for the scroll check. ".repeat(5)}`,
    turnId: "live-turn",
  }));
  let agent = {
    ...lead,
    status: "running",
    inFlight: true,
    turnId: "live-turn",
    activity: { phase: "writing" },
  };
  const transcript = () => ({
    agent,
    items,
    order: items.map((i) => i.id),
    replace: true,
  });
  browser = await chromium.launch({
    headless: true,
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  });
  const initialItems = structuredClone(items);
  const initialAgent = structuredClone(agent);
  const records = [];
  for (const width of [1440, 390]) {
    items = structuredClone(initialItems);
    agent = structuredClone(initialAgent);
    const page = await browser.newPage({ viewport: { width, height: 960 } });
    page.setDefaultTimeout(10000);
    const errors = [];
    page.on("pageerror", (e) => errors.push(e.message));
    await page.addInitScript(() => {
      window.motionStreams = [];
      window.motionScrollTrace = [];
      document.addEventListener(
        "scroll",
        (event) => {
          if (event.target?.id === "messages")
            window.motionScrollTrace.push({
              top: event.target.scrollTop,
              height: event.target.scrollHeight,
              time: performance.now(),
            });
        },
        true,
      );
      window.EventSource = class extends EventTarget {
        constructor(url) {
          super();
          this.url = url;
          window.motionStreams.push(this);
        }
        close() {
          window.motionStreams = window.motionStreams.filter((s) => s !== this);
        }
      };
      window.motionEmit = (id, data) => {
        for (const s of window.motionStreams)
          if (new URL(s.url, location.href).searchParams.get("id") === id)
            s.onmessage?.({ data: JSON.stringify(data) });
      };
    });
    // These controlled snapshots use the HTTP path, not fixture replication.
    await page.route("**/api/sync/**", (route) =>
      route.fulfill({ status: 404, json: { error: "HTTP fixture" } }),
    );
    await page.route(/\/api\/state(?:\?.*)?$/, (r) =>
      r.fulfill({
        json: {
          ...state,
          threads: state.threads.map((a) => (a.id === lead.id ? agent : a)),
          runtime: { ...state.runtime, requests: [] },
        },
      }),
    );
    await page.route("**/api/transcript?*", (r) =>
      r.fulfill({
        json:
          new URL(r.request().url()).searchParams.get("id") === lead.id
            ? transcript()
            : { items: [], agent: other },
      }),
    );
    let pendingSend;
    let resolveSendRoute;
    const sendRouteCaptured = new Promise((resolve) => {
      resolveSendRoute = resolve;
    });
    await page.route(/\/api\/messages(?:\?.*)?$/, (r) => {
      pendingSend = r;
      resolveSendRoute(r);
    });
    await page.goto(origin);

    if (width < 700) await page.locator("#sidebar-toggle").click();
    await page.locator(`[data-chat="${lead.id}"]`).click();
    const emit = async () => {
      await page.evaluate(({ id, data }) => window.motionEmit(id, data), {
        id: lead.id,
        data: transcript(),
      });
    };
    await emit();
    const rootSelector = `#messages[data-motion-root="${width}"]`;
    const markTranscriptRoot = async () => {
      const visibleRoot = page.locator(
        "#conversation #messages:visible:has([data-message=note35])",
      );
      await visibleRoot.locator('[data-message="note35"]').waitFor();
      await visibleRoot.evaluate((root, id) => {
        root.dataset.motionRoot = String(id);
      }, width);
    };
    await markTranscriptRoot();
    const transcriptRoot = page.locator(rootSelector);
    const conversationSelector = `#conversation:has(${rootSelector})`;
    const composerSelector = `${conversationSelector} #message`;
    const composer = page.locator(composerSelector);
    const gap = () =>
      page
        .locator(rootSelector)
        .evaluate((e) => e.scrollHeight - e.scrollTop - e.clientHeight);
    assert.ok((await gap()) < 2, "initial history follows bottom");
    await page.locator(rootSelector).evaluate((root) => {
      root.scrollTop -= 18;
      root.dispatchEvent(new Event("scroll", { bubbles: true }));
    });
    await afterPaint(page, rootSelector);
    await composer.fill("A small draft");
    await afterPaint(page, rootSelector);
    assert.ok(
      Math.abs((await gap()) - 18) < 2,
      "A render preserves the reader's small bottom distance",
    );
    await page.locator(rootSelector).evaluate((root) => {
      root.scrollTop = root.scrollHeight;
      root.dispatchEvent(new Event("scroll", { bubbles: true }));
    });

    await page
      .locator(composerSelector)
      .fill(Array.from({ length: 6 }, (_, i) => `Draft line ${i}`).join("\n"));
    await afterPaint(page, rootSelector);
    assert.ok((await gap()) < 2, "growing composer retains bottom");
    await page.route("**/api/sync/identity", (route) =>
      route.fulfill({ json: syncIdentity }),
    );
    await page.locator(`${conversationSelector} #send`).click();
    await sendRouteCaptured;
    assert.ok(pendingSend, "send request captured");
    items.push({
      id: `sent-${width}`,
      role: "user",
      text: pendingSend.request().postDataJSON().text,
      turnId: "live-turn",
    });
    await emit();
    await pendingSend.fulfill({
      json: { status: "saved", deliveries: { [lead.id]: "queued" } },
    });
    await page.waitForFunction(
      (selector) => document.querySelector(selector).value === "",
      composerSelector,
    );
    await afterPaint(page, rootSelector);
    assert.ok((await gap()) < 2, "send and composer shrink retain bottom");
    await page.waitForLoadState("networkidle");
    const scrollBefore = await page.locator(rootSelector).evaluate(
      (root) =>
        new Promise((resolve, reject) => {
          let previous = "";
          let stableFrames = 0;
          const deadline = performance.now() + 10000;
          const sample = () => {
            const noteCount = root.querySelectorAll(
              '[data-message^="note"]',
            ).length;
            const bottomGap =
              root.scrollHeight - root.scrollTop - root.clientHeight;
            const values = [
              noteCount,
              root.scrollTop,
              root.scrollHeight,
              root.clientHeight,
            ].join(":");
            const ready = noteCount === 36 && bottomGap < 2 && bottomGap >= 0;
            stableFrames = ready && values === previous ? stableFrames + 1 : 0;
            previous = values;
            if (stableFrames >= 3) {
              const before = root.scrollTop;
              root.scrollTop = Math.max(0, before - 1200);
              root.dispatchEvent(new Event("scroll", { bubbles: true }));
              resolve({
                before,
                top: root.scrollTop,
                height: root.scrollHeight,
                clientHeight: root.clientHeight,
              });
            } else if (performance.now() >= deadline) {
              reject(
                new Error("Transcript did not settle at its latest message."),
              );
            } else requestAnimationFrame(sample);
          };
          requestAnimationFrame(sample);
        }),
    );
    assert.ok(
      scrollBefore.top < scrollBefore.before &&
        scrollBefore.height - scrollBefore.top - scrollBefore.clientHeight >=
          32,
      `reader leaves the bottom before stream (${JSON.stringify(scrollBefore)})`,
    );
    try {
      await page.locator(`${conversationSelector} #jump-latest`).waitFor();
    } catch (error) {
      const state = await page.locator(rootSelector).evaluate(
        (root, selector) => ({
          scrollTop: root.scrollTop,
          scrollHeight: root.scrollHeight,
          clientHeight: root.clientHeight,
          gap: root.scrollHeight - root.scrollTop - root.clientHeight,
          button: !!document.querySelector(selector),
        }),
        `${conversationSelector} #jump-latest`,
      );
      throw new Error(
        `Latest button did not appear: ${JSON.stringify(state)}`,
        {
          cause: error,
        },
      );
    }
    const anchor = await page.locator(rootSelector).evaluate((root) => {
      const bounds = root.getBoundingClientRect();
      const p = [...root.querySelectorAll("[data-message] p")].find((p) => {
        const box = p.getBoundingClientRect();
        return (
          box.height > 0 && box.bottom > bounds.top && box.top < bounds.bottom
        );
      });
      const message = p.closest("[data-message]");
      return {
        messageId: message.dataset.message,
        paragraph: [...message.querySelectorAll("p")].indexOf(p),
        top: p.getBoundingClientRect().top - bounds.top,
        scrollTop: root.scrollTop,
        scrollHeight: root.scrollHeight,
      };
    });
    const anchorParagraph = page
      .locator(`${rootSelector} [data-message="${anchor.messageId}"] p`)
      .nth(anchor.paragraph);
    const waitForStableAnchor = () =>
      page.waitForFunction(
        ({ rootSelector, messageId, paragraph }) =>
          new Promise((resolve) => {
            let previous = Number.NaN;
            let stableFrames = 0;
            const sample = () => {
              const root = document.querySelector(rootSelector);
              const message = Array.from(
                root.querySelectorAll("[data-message]"),
              ).find((item) => item.dataset.message === messageId);
              const element = message?.querySelectorAll("p")[paragraph];
              if (!element) {
                requestAnimationFrame(sample);
                return;
              }
              const top =
                element.getBoundingClientRect().top -
                root.getBoundingClientRect().top;
              stableFrames =
                Math.abs(top - previous) < 0.1 ? stableFrames + 1 : 0;
              previous = top;
              if (stableFrames === 3) resolve(true);
              else requestAnimationFrame(sample);
            };
            requestAnimationFrame(sample);
          }),
        {
          rootSelector,
          messageId: anchor.messageId,
          paragraph: anchor.paragraph,
        },
      );
    const anchorY = () =>
      anchorParagraph.evaluate((e) => {
        const root = e.closest("#messages");
        return e.getBoundingClientRect().top - root.getBoundingClientRect().top;
      });
    items.push({
      id: `stream-${width}`,
      role: "assistant",
      text: "Another finished paragraph.\n\nMore incoming",
      streaming: true,
      turnId: "live-turn",
    });
    await page.evaluate(() => (window.motionScrollTrace = []));
    await emit();
    await transcriptRoot
      .locator(`[data-message="stream-${width}"]`)
      .waitFor({ state: "visible" });
    await afterPaint(page, rootSelector);
    await waitForStableAnchor();
    const streamAnchorTop = await anchorY();
    const streamDebug = await transcriptRoot.evaluate((root, messageId) => {
      const message = [...root.querySelectorAll("[data-message]")].find(
        (node) => node.dataset.message === messageId,
      );
      return {
        scrollTop: root.scrollTop,
        scrollHeight: root.scrollHeight,
        clientHeight: root.clientHeight,
        bottomGap: root.scrollHeight - root.scrollTop - root.clientHeight,
        jumpLatest: !!document.querySelector("#jump-latest"),
        stream: message?.outerHTML.slice(0, 500),
        scrollTrace: window.motionScrollTrace,
      };
    }, anchor.messageId);
    assert.ok(
      Math.abs(streamAnchorTop - anchor.top) < 2,
      `stream does not move reader (expected ${anchor.top}, got ${streamAnchorTop}; ${JSON.stringify(streamDebug)})`,
    );
    await composer.fill("A\nB\nC\nD\nE\nF");
    await afterPaint(page, rootSelector);
    await waitForStableAnchor();
    const expandedAnchorTop = await anchorY();
    assert.ok(
      Math.abs(expandedAnchorTop - anchor.top) < 2,
      `composer expansion does not move reader (${JSON.stringify({ anchor, expandedAnchorTop, scrollTop: await transcriptRoot.evaluate((root) => root.scrollTop) })})`,
    );
    await composer.fill("");
    await afterPaint(page, rootSelector);
    await waitForStableAnchor();
    assert.ok(
      Math.abs((await anchorY()) - anchor.top) < 2,
      "composer contraction does not move reader",
    );
    // Async image/layout growth above the reading point must preserve this paragraph.
    const earlyMessage = transcriptRoot.locator('[data-message="note0"]');
    const earlyMessageHeight = await earlyMessage.evaluate(
      (e) => e.getBoundingClientRect().height,
    );
    const scrollBeforeGrowth = await page
      .locator(rootSelector)
      .evaluate((root) => root.scrollTop);
    await earlyMessage.evaluate((e) => {
      e.style.paddingTop = "180px";
    });
    await page.waitForFunction(
      ({ id, minHeight, selector }) => {
        const root = document.querySelector(selector);
        const message = root?.querySelector(
          `[data-message="${CSS.escape(id)}"]`,
        );
        return (
          message &&
          getComputedStyle(message).paddingTop === "180px" &&
          message.getBoundingClientRect().height > minHeight
        );
      },
      { id: "note0", minHeight: earlyMessageHeight, selector: rootSelector },
    );
    await afterPaint(page, rootSelector);
    await waitForStableAnchor();
    const lateAnchorTop = await anchorY();
    const scrollAfterGrowth = await page
      .locator(rootSelector)
      .evaluate((root) => root.scrollTop);
    assert.ok(
      Math.abs(lateAnchorTop - anchor.top) < 2,
      `late content height above reader is anchored (expected ${anchor.top}, got ${lateAnchorTop}, scroll ${scrollBeforeGrowth} to ${scrollAfterGrowth})`,
    );
    await earlyMessage.evaluate((e) => {
      e.style.paddingTop = "";
    });
    await page.waitForFunction(
      ({ id, maxHeight, selector }) =>
        document
          .querySelector(selector)
          ?.querySelector(`[data-message="${CSS.escape(id)}"]`)
          ?.getBoundingClientRect().height <= maxHeight,
      {
        id: "note0",
        maxHeight: earlyMessageHeight + 1,
        selector: rootSelector,
      },
    );
    await afterPaint(page, rootSelector);
    await waitForStableAnchor();
    await transcriptRoot
      .locator('[data-message="note35"]')
      .evaluate((e) => (e.dataset.retained = "yes"));
    items = items.map((i) => ({
      ...i,
      streaming: false,
      turnStatus: "completed",
    }));
    agent = {
      ...agent,
      status: "completed",
      inFlight: false,
      turnId: null,
      activity: null,
    };
    await emit();
    await transcriptRoot
      .locator('[data-turn="live-turn"][data-outcome="completed"]')
      .first()
      .waitFor();
    await afterPaint(page, rootSelector);
    await waitForStableAnchor();
    assert.equal(
      await transcriptRoot
        .locator('[data-message="note35"]')
        .getAttribute("data-retained"),
      "yes",
      "completion retains live message nodes",
    );
    assert.ok(
      Math.abs((await anchorY()) - anchor.top) < 2,
      "completion does not rearrange reader history",
    );
    const beforeSwitch = await page
      .locator(rootSelector)
      .evaluate((e) => e.scrollTop);
    await transcriptRoot.evaluate((root) => delete root.dataset.motionRoot);
    if (width < 700) await page.locator("#sidebar-toggle").click();
    await page.locator(`[data-chat="${other.id}"]`).click();
    await page.evaluate(
      ({ id }) =>
        window.motionEmit(id, { items: [], order: [], replace: true }),
      { id: other.id },
    );
    if (width < 700) await page.locator("#sidebar-toggle").click();
    await page.locator(`[data-chat="${lead.id}"]`).click();
    await emit();
    await markTranscriptRoot();
    await afterPaint(page, rootSelector);
    const restoredCompletion = await page
      .locator(rootSelector)
      .evaluate((e) => ({
        scrollTop: e.scrollTop,
        scrollHeight: e.scrollHeight,
        clientHeight: e.clientHeight,
      }));
    assert.ok(
      Math.abs(restoredCompletion.scrollTop - beforeSwitch) < 2,
      `completed chat retains chronological layout and reading position (${JSON.stringify({ beforeSwitch, restoredCompletion })})`,
    );
    items = items.map(({ turnStatus, ...i }) => i);
    agent = {
      ...agent,
      status: "running",
      inFlight: true,
      turnId: "live-turn",
    };
    await emit();
    await page.locator(rootSelector).evaluate((e) => {
      e.scrollTop = 1400;
      e.dispatchEvent(new Event("scroll", { bubbles: true }));
    });
    await afterPaint(page, rootSelector);
    const stored = await page
      .locator(rootSelector)
      .evaluate((e) => e.scrollTop);
    await transcriptRoot.evaluate((root) => delete root.dataset.motionRoot);
    if (width < 700) await page.locator("#sidebar-toggle").click();
    await page.locator(`[data-chat="${other.id}"]`).click();
    await page.evaluate(
      ({ id }) =>
        window.motionEmit(id, { items: [], order: [], replace: true }),
      { id: other.id },
    );
    if (width < 700) await page.locator("#sidebar-toggle").click();
    await page.locator(`[data-chat="${lead.id}"]`).click();
    await emit();
    await markTranscriptRoot();
    await afterPaint(page, rootSelector);
    assert.ok(
      Math.abs(
        (await page.locator(rootSelector).evaluate((e) => e.scrollTop)) -
          stored,
      ) < 2,
      "chat switch restores reading position",
    );
    await page.locator(`${conversationSelector} #jump-latest`).click();
    await afterPaint(page, rootSelector);
    assert.ok((await gap()) < 2, "Latest resumes following");
    items.push({
      id: `tail-${width}`,
      role: "assistant",
      text: "Tail paragraph. ".repeat(100),
      turnId: "live-turn",
    });
    await emit();
    assert.ok((await gap()) < 2, "new text follows after Latest");
    assert.deepEqual(errors, []);
    records.push({
      width,
      anchor: anchor.top,
      streamAnchorAfter: streamAnchorTop,
      streamScrollTop: {
        before: scrollBefore.top,
        after: streamDebug.scrollTop,
      },
      restored: stored,
      beforeSwitch,
      bottomGap: await gap(),
    });
    await page.screenshot({ path: join(dir, `conversation-${width}.png`) });
    await page.close();
  }
  await writeFile(join(dir, "geometry.json"), JSON.stringify(records, null, 2));
  console.log(JSON.stringify({ ok: true, evidence: dir, records }));
} finally {
  await browser?.close();
  proc.kill();
}
