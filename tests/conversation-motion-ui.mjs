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
const pause = (ms) => new Promise((r) => setTimeout(r, ms));
try {
  const port = await new Promise((resolve, reject) => {
    proc.stdout.once("data", (d) => resolve(Number(String(d).trim())));
    proc.once("exit", () => reject(Error(log)));
  });
  const origin = `http://127.0.0.1:${port}`;
  const state = await (await fetch(origin + "/api/state")).json();
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
    await page.route("**/api/sync/**", (r) =>
      r.fulfill({ status: 404, json: { error: "HTTP fixture" } }),
    );
    await page.route("**/api/state", (r) =>
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
    await page.route("**/api/messages", (r) => {
      pendingSend = r;
    });
    await page.goto(origin);

    if (width < 700) await page.locator("#sidebar-toggle").click();
    await page.locator(`[data-chat="${lead.id}"]`).click();
    const emit = async () => {
      await page.evaluate(({ id, data }) => window.motionEmit(id, data), {
        id: lead.id,
        data: transcript(),
      });
      await pause(100);
    };
    await emit();
    await page.locator('[data-message="note35"]').waitFor();
    const gap = () =>
      page
        .locator("#messages")
        .evaluate((e) => e.scrollHeight - e.scrollTop - e.clientHeight);
    assert.ok((await gap()) < 2, "initial history follows bottom");
    await page
      .locator("#message")
      .fill(Array.from({ length: 6 }, (_, i) => `Draft line ${i}`).join("\n"));
    await pause(100);
    assert.ok((await gap()) < 2, "growing composer retains bottom");
    await page.locator("#send").click();
    for (let i = 0; !pendingSend && i < 100; i++) await pause(10);
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
      () => document.querySelector("#message").value === "",
    );
    await pause(100);
    assert.ok((await gap()) < 2, "send and composer shrink retain bottom");
    await page.locator("#messages").evaluate((e) => {
      e.scrollTop = 1200;
      e.dispatchEvent(new Event("scroll", { bubbles: true }));
    });
    await page.locator("#jump-latest").waitFor();
    const anchor = await page.locator("#messages").evaluate((root) => {
      const p = [...root.querySelectorAll("[data-message] p")].find(
        (p) =>
          p.getBoundingClientRect().top >= root.getBoundingClientRect().top,
      );
      p.dataset.motionAnchor = "yes";
      return p.getBoundingClientRect().top;
    });
    const anchorY = () =>
      page
        .locator('[data-motion-anchor="yes"]')
        .evaluate((e) => e.getBoundingClientRect().top);
    items.push({
      id: `stream-${width}`,
      role: "assistant",
      text: "Another finished paragraph.\n\nMore incoming",
      streaming: true,
      turnId: "live-turn",
    });
    await emit();
    assert.ok(
      Math.abs((await anchorY()) - anchor) < 2,
      "stream does not move reader",
    );
    await page.locator("#message").fill("A\nB\nC\nD\nE\nF");
    await pause(100);
    assert.ok(
      Math.abs((await anchorY()) - anchor) < 2,
      "composer expansion does not move reader",
    );
    await page.locator("#message").fill("");
    await pause(100);
    assert.ok(
      Math.abs((await anchorY()) - anchor) < 2,
      "composer contraction does not move reader",
    );
    // Async image/layout growth above the reading point must preserve this paragraph.
    await page.locator('[data-message="note0"]').evaluate((e) => {
      e.style.paddingTop = "180px";
    });
    await pause(100);
    assert.ok(
      Math.abs((await anchorY()) - anchor) < 2,
      "late content height above reader is anchored",
    );
    await page.locator('[data-message="note0"]').evaluate((e) => {
      e.style.paddingTop = "";
    });
    await pause(100);
    await page
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
    assert.equal(
      await page
        .locator('[data-message="note35"]')
        .getAttribute("data-retained"),
      "yes",
      "completion retains live message nodes",
    );
    assert.ok(
      Math.abs((await anchorY()) - anchor) < 2,
      "completion does not rearrange reader history",
    );
    const beforeSwitch = await page
      .locator("#messages")
      .evaluate((e) => e.scrollTop);
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
    assert.ok(
      Math.abs(
        (await page.locator("#messages").evaluate((e) => e.scrollTop)) -
          beforeSwitch,
      ) < 2,
      "completed chat retains chronological layout and reading position",
    );
    items = items.map(({ turnStatus, ...i }) => i);
    agent = {
      ...agent,
      status: "running",
      inFlight: true,
      turnId: "live-turn",
    };
    await emit();
    await page.locator("#messages").evaluate((e) => {
      e.scrollTop = 1400;
      e.dispatchEvent(new Event("scroll", { bubbles: true }));
    });
    await pause(100);
    const stored = await page.locator("#messages").evaluate((e) => e.scrollTop);
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
    assert.ok(
      Math.abs(
        (await page.locator("#messages").evaluate((e) => e.scrollTop)) - stored,
      ) < 2,
      "chat switch restores reading position",
    );
    await page.locator("#jump-latest").click();
    await pause(100);
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
      anchor,
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
