#!/usr/bin/env node
// Production workspace geometry with long labels and an active agent panel.
// The fixture uses an isolated runtime. No model service or user state is used.
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
const root = await mkdtemp(join(tmpdir(), "codex-workspace-layout-"));
const fixture = spawn(
  "python3",
  ["-B", join(repo, "tests/simple-ui-fixture.py"), root],
  { cwd: repo, stdio: ["pipe", "pipe", "pipe"] },
);
let browser;
let log = "";
fixture.stderr.on("data", (chunk) => (log += chunk));

const longTitle =
  "A very long conversation title that tests header compression at every supported viewport width";
const longProject =
  "/Users/igor/Projects/a-super-long-project-name-for-layout-validation-with-many-words";
const longProjectName = longProject.split("/").at(-1);
const longAccount =
  "account-owner-with-a-very-long-email-address-for-layout@example.test";
const panelHeight = 150;
const viewports = [
  { width: 1440, height: 900, name: "desktop-1440x900" },
  { width: 1280, height: 800, name: "desktop-1280x800" },
  { width: 1024, height: 768, name: "desktop-1024x768" },
  { width: 390, height: 844, name: "mobile-390x844" },
];

const port = await new Promise((resolve, reject) => {
  fixture.stdout.once("data", (chunk) => resolve(Number(String(chunk).trim())));
  fixture.once("exit", () => reject(Error(log || "Fixture exited early")));
});
const origin = `http://127.0.0.1:${port}`;
const initial = await (await fetch(origin + "/api/state")).json();
const fixtureAccounts = await (await fetch(origin + "/api/accounts")).json();
const lead = initial.threads.find((agent) => agent.name === "Release lead");
assert.ok(lead, "fixture has a lead chat");

const panel = {
  agent: lead.id,
  version: 1,
  dataVersion: 1,
  format: "html",
  html: `<main class="layout-panel"><strong>Active agent panel</strong><p>Panel content stays inside the shared reading column.</p><p>Additional content stays within the panel.</p></main>`,
  css: `.layout-panel { min-height: 100px; padding: 12px 16px; } .layout-panel p { margin: 4px 0; }`,
  updated: Date.now() / 1000,
  callbacks: [],
  submittedCallbacks: [],
};

try {
  browser = await chromium.launch({
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    headless: true,
    args: ["--disable-extensions", "--no-first-run"],
  });
  const page = await browser.newPage({ viewport: viewports[0] });
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));

  // Keep the browser on the HTTP projection. This avoids a local sync cache
  // replacing the long-label response with the fixture's short labels.
  await page.route("**/api/sync/identity", (route) =>
    route.fulfill({
      status: 404,
      json: { error: "Sync unavailable in fixture" },
    }),
  );
  await page.route("**/api/state", async (route) => {
    const response = await route.fetch();
    const data = await response.json();
    const target =
      data.threads?.find((agent) => agent.id === lead.id) ||
      data.threads?.find((agent) => agent.name === "Release lead");
    const targetId = target?.id || lead.id;
    for (const agent of data.threads || []) {
      if (agent.id === targetId) {
        agent.name = longTitle;
        agent.cwd = longProject;
        agent.panelVersion = panel.version;
        agent.panelDataVersion = panel.dataVersion;
      } else if (agent.rootId === targetId) {
        agent.cwd = longProject;
      }
    }
    for (const agent of data.runtime?.agents || []) {
      if (agent.id === targetId) {
        agent.name = longTitle;
        agent.cwd = longProject;
        agent.panelVersion = panel.version;
        agent.panelDataVersion = panel.dataVersion;
      } else if (agent.rootId === targetId) {
        agent.cwd = longProject;
      }
    }
    await route.fulfill({ response, json: data });
  });
  await page.route("**/api/accounts", async (route) => {
    const data = structuredClone(fixtureAccounts);
    const account = data.accounts.find((item) => item.id === "default");
    account.email = longAccount;
    account.label = "Account owner with a very long label";
    await route.fulfill({ json: data });
  });
  await page.route("**/api/panel?**", (route) =>
    route.fulfill({ json: panel }),
  );

  await page.goto(origin);
  await page.locator("[data-chat]").filter({ hasText: longTitle }).click();
  await page.getByText("I assigned 40 workers", { exact: false }).waitFor();
  await page.locator(".agent-panel[data-ready='true']").waitFor();

  const measure = () =>
    page.evaluate(() => {
      const box = (selector) => {
        const element = document.querySelector(selector);
        if (!element) return null;
        const rect = element.getBoundingClientRect();
        const style = getComputedStyle(element);
        return {
          x: rect.x,
          y: rect.y,
          width: rect.width,
          height: rect.height,
          right: rect.right,
          bottom: rect.bottom,
          clientWidth: element.clientWidth,
          scrollWidth: element.scrollWidth,
          clientHeight: element.clientHeight,
          scrollHeight: element.scrollHeight,
          minHeight: style.minHeight,
          computedHeight: style.height,
          paddingLeft: Number.parseFloat(style.paddingLeft) || 0,
          paddingRight: Number.parseFloat(style.paddingRight) || 0,
          text: element.textContent?.trim() || "",
        };
      };
      const messages = box("#messages");
      const transcript = messages
        ? {
            left: messages.x + messages.paddingLeft,
            right: messages.right - messages.paddingRight,
          }
        : null;
      const header = document.querySelector(".workspace-header");
      const headerRect = header?.getBoundingClientRect();
      const headerChildren = header
        ? [...header.querySelectorAll(":scope > *")]
            .filter((element) => getComputedStyle(element).display !== "none")
            .map((element) => {
              const rect = element.getBoundingClientRect();
              return { left: rect.left, right: rect.right };
            })
        : [];
      return {
        viewport: { width: innerWidth, height: innerHeight },
        document: {
          scrollWidth: document.documentElement.scrollWidth,
          clientWidth: document.documentElement.clientWidth,
          bodyScrollWidth: document.body.scrollWidth,
        },
        header: {
          ...box(".workspace-header"),
          overflow: header ? header.scrollWidth - header.clientWidth : 0,
          children: headerChildren,
          left: headerRect?.left || 0,
          right: headerRect?.right || 0,
        },
        title: box("#conversation-title"),
        heading: box(".conversation-heading"),
        account: box(".account-picker"),
        project: box("#project"),
        settings: box(".conversation-settings"),
        messages,
        transcript,
        panel: box(".agent-panel"),
        composer: box("#composer"),
        message: box("#message"),
        usage: box(".usage-footer"),
        shortcuts: box(".workspace-shortcuts"),
      };
    });

  const measurements = [];
  for (const viewport of viewports) {
    await page.setViewportSize(viewport);
    await page.waitForTimeout(80);
    const current = await measure();
    measurements.push(current);
    await writeFile(
      join(root, "measurements.json"),
      JSON.stringify(measurements, null, 2),
    );
    await page.screenshot({
      path: join(root, `workspace-${viewport.name}.png`),
      animations: "disabled",
    });
    const prefix = viewport.name;
    assert.equal(
      await page.locator("#conversation-title").getAttribute("title"),
      longTitle,
      `${prefix}: full title is available to assistive and hover users`,
    );
    assert.ok(
      current.title?.width >= 180,
      `${prefix}: title width ${current.title?.width}`,
    );
    if (viewport.width > 760) {
      assert.equal(
        current.account?.text,
        longAccount,
        `${prefix}: account label`,
      );
      assert.equal(
        current.project?.text,
        longProjectName,
        `${prefix}: project label`,
      );
    }
    assert.ok(
      current.document.scrollWidth <= current.viewport.width + 1 &&
        current.document.bodyScrollWidth <= current.viewport.width + 1,
      `${prefix}: document horizontal overflow`,
    );
    assert.ok(
      current.header.overflow <= 1 &&
        current.header.children.every(
          ({ left, right }) =>
            left >= current.header.left - 1 &&
            right <= current.header.right + 1,
        ),
      `${prefix}: header horizontal overflow`,
    );
    assert.ok(
      current.messages.height >= viewport.height * 0.48,
      `${prefix}: transcript height ${current.messages.height}`,
    );
    assert.ok(
      Number.parseFloat(current.message.minHeight) <= 36 &&
        current.message.height <= 40,
      `${prefix}: blank textarea height ${current.message.height} (${current.message.minHeight})`,
    );
    assert.ok(
      current.composer.height <= (viewport.width <= 760 ? 110 : 90),
      `${prefix}: blank composer height ${current.composer.height}`,
    );
    assert.equal(
      await page.locator("#messages #requests").count(),
      1,
      `${prefix}: requests scroll with the transcript`,
    );
    assert.equal(
      await page.locator("#conversation > .user-tasks-compact").count(),
      0,
      `${prefix}: tasks do not reserve transcript space`,
    );
    assert.equal(
      await page.locator("#composer .prompt-navigation").count(),
      1,
      `${prefix}: prompt history shares the composer toolbar`,
    );
    assert.ok(current.panel, `${prefix}: active panel is present`);
    assert.ok(
      current.panel.height > 0 && current.panel.height <= panelHeight + 1,
      `${prefix}: panel height ${current.panel.height}`,
    );
    const panelFrame = page.locator(".agent-panel iframe");
    assert.equal(
      await panelFrame.evaluate(
        (element) => element.getBoundingClientRect().height,
      ),
      panelHeight,
      `${prefix}: active panel frame height`,
    );
    const transcriptWidth = current.transcript.right - current.transcript.left;
    assert.ok(transcriptWidth <= 681, `${prefix}: readable text width`);
    assert.ok(
      Math.abs(
        current.transcript.left +
          current.transcript.right -
          current.composer.x -
          current.composer.right,
      ) <= 2,
      `${prefix}: text stays centered above the composer`,
    );
    assert.ok(
      Math.abs(current.panel.x - current.composer.x) <= 1 &&
        Math.abs(current.panel.right - current.composer.right) <= 1,
      `${prefix}: panel and composer share their edges`,
    );
    if (viewport.width === 1024) {
      await page.getByRole("button", { name: /^Team/ }).click();
      const drawer = page.locator(".mantine-Drawer-content:visible");
      await drawer.waitFor();
      await page.waitForTimeout(300);
      const drawerBox = await drawer.boundingBox();
      assert.ok(drawerBox, "team drawer has geometry");
      assert.ok(
        drawerBox.x >= -1 &&
          drawerBox.x + drawerBox.width <= viewport.width + 1,
        "team drawer stays inside the viewport",
      );
      const open = await measure();
      assert.ok(
        open.document.scrollWidth <= viewport.width + 1 &&
          open.header.overflow <= 1,
        "open team drawer does not create horizontal overflow",
      );
      await page.screenshot({
        path: join(root, "workspace-team-open-1024x768.png"),
        animations: "disabled",
      });
      await page.keyboard.press("Escape");
      await drawer.waitFor({ state: "hidden" });
    }
  }

  await writeFile(
    join(root, "workspace-layout.json"),
    JSON.stringify(measurements, null, 2),
  );
  assert.deepEqual(errors, []);
  console.log(
    `PASS workspace layout: long title, project, account, capped panel, narrow text, aligned composer, blank composer, and team drawer across 1440, 1280, 1024, and 390px. Evidence ${root}`,
  );
} finally {
  for (const context of browser?.contexts() || []) {
    for (const page of context.pages())
      await page.unrouteAll({ behavior: "wait" });
  }
  await browser?.close();
  fixture.kill("SIGTERM");
}
