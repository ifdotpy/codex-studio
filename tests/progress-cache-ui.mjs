#!/usr/bin/env node
// Production renderer, real progress files, isolated backend. No model calls.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { mkdir, mkdtemp, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { createRequire } from "node:module";

const repo = join(import.meta.dirname, "..");
const { chromium, webkit } = createRequire(join(repo, "web/package.json"))(
  "playwright-core",
);
const browserType = process.env.BROWSER === "webkit" ? webkit : chromium;
const root = await mkdtemp(join(tmpdir(), "studio-progress-cache-ui-"));
const models = ["gpt-6-astra", "gpt-5.6-sol", "gpt-5.6-luna"].map((model) => ({
  model,
  defaultReasoningEffort: "medium",
  supportedReasoningEfforts: (model === "gpt-5.6-luna"
    ? ["low", "medium", "high", "max"]
    : ["low", "medium", "high", "ultra"]
  ).map((reasoningEffort) => ({ reasoningEffort })),
  serviceTiers: [{ id: "priority" }],
}));
const proc = spawn(
  process.env.PYTHON || "/opt/homebrew/bin/python3",
  ["-B", join(repo, "tests/simple-ui-fixture.py"), root],
  {
    stdio: ["pipe", "pipe", "pipe"],
    env: {
      ...process.env,
      EXECUTION_SETTINGS_CATALOG: JSON.stringify(models),
      CODEX_BOARD_STATE_DIR: join(root, "board"),
    },
  },
);
let browser,
  page,
  log = "";
proc.stderr.on("data", (data) => {
  log += data;
});
const diagnostics = [];
const until = async (test, label) => {
  for (let i = 0; i < 160; i++) {
    if (await test()) return;
    await new Promise((resolve) => setTimeout(resolve, 75));
  }
  throw Error(label + "\n" + log);
};
try {
  const port = await new Promise((resolve, reject) => {
    const timer = setTimeout(
      () => reject(Error("Fixture startup timed out: " + log)),
      30000,
    );
    proc.stdout.once("data", (data) => {
      clearTimeout(timer);
      resolve(Number(String(data).trim()));
    });
    proc.once("exit", () => {
      clearTimeout(timer);
      reject(Error(log || "Fixture exited"));
    });
  });
  const origin = `http://127.0.0.1:${port}`;
  const initial = await (await fetch(origin + "/api/state")).json();
  const lead = initial.threads.find((agent) => agent.name === "Release lead");
  const other = initial.threads.find((agent) => agent.name === "Other project");
  const readPanel = (agent) =>
    fetch(`${origin}/api/panel?agent=${agent.id}`).then((response) =>
      response.json(),
    );
  const files = new Map();
  for (const agent of [lead, other]) {
    const response = await readPanel(agent);
    assert.ok(
      response.path.startsWith(root + "/"),
      "Only task-owned progress files may change",
    );
    files.set(agent.id, response.path);
    await mkdir(dirname(response.path), { recursive: true });
  }
  const texts = {
    lead: "Lead cached progress.",
    other: "Other prefetched progress.",
    update: "Lead updated progress.",
  };
  await writeFile(files.get(lead.id), texts.lead);
  await writeFile(files.get(other.id), texts.other);
  browser = await browserType.launch({
    headless: true,
    ...(browserType === chromium
      ? {
          executablePath:
            process.env.CHROME_BIN ||
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        }
      : {}),
  });
  // Transport fault injection must remain observable after reload in WebKit.
  const context = await browser.newContext({
    viewport: { width: 1440, height: 960 },
    serviceWorkers: "block",
  });
  page = await context.newPage();
  page.setDefaultTimeout(12000);
  const errors = [],
    responses = [],
    held = [];
  let hold = false;
  page.on("pageerror", (error) => {
    errors.push(error.message);
    diagnostics.push({ error: error.message, at: Date.now() });
  });
  page.on("response", async (response) => {
    const url = new URL(response.url());
    if (url.pathname !== "/api/panel" || response.request().method() !== "GET")
      return;
    try {
      responses.push(await response.json());
    } catch {
      /* Navigation can cancel an old response. */
    }
  });
  await page.route("**/api/panel?*", async (route) => {
    if (hold) {
      held.push(route);
      return;
    }
    await route.continue();
  });
  const release = async () => {
    hold = false;
    await Promise.all(
      held.splice(0).map((route) => route.continue().catch(() => {})),
    );
  };
  await context.addInitScript(
    ({ id, stateDir }) => {
      localStorage.setItem(
        `codex-desktop-opened:${stateDir}`,
        JSON.stringify(id),
      );
      localStorage.setItem("codex-mobile-opened", JSON.stringify(id));
    },
    { id: lead.id, stateDir: initial.stateDir },
  );
  await page.goto(origin);
  await page.locator("#message").waitFor();
  await page.evaluate(() => document.fonts.ready);
  const panel = () =>
    page.getByRole("region", { name: "Agent progress", exact: true });
  const currentText = () =>
    panel().locator(".agent-panel-current, .agent-panel-saved");
  await currentText().getByText(texts.lead, { exact: true }).waitFor();
  await until(
    () =>
      responses.some(
        (response) =>
          response.agent === other.id && response.markdown === texts.other,
      ),
    "An unvisited chat prefetches its actual progress file",
  );
  assert.equal(
    await page.locator(`[data-chat="${lead.id}"]`).getAttribute("aria-current"),
    "true",
  );
  console.log(
    "PASS unvisited chat progress loads in the existing background prefetch",
  );
  await page.evaluate(
    ({ lead, other }) => {
      window.progressFlashes = [];
      new MutationObserver(() => {
        const selected = document.querySelector(
          '[data-chat][aria-current="true"]',
        );
        const panel = document.querySelector(".agent-panel");
        const text = panel?.querySelector(
          ".agent-panel-current, .agent-panel-saved",
        )?.textContent;
        if (!selected || !panel || !text) return;
        const id = selected.getAttribute("data-chat");
        const correct =
          panel.getAttribute("data-agent") === id &&
          (id === lead
            ? text.startsWith("Lead ")
            : id === other
              ? text.startsWith("Other ")
              : true);
        if (!correct)
          window.progressFlashes.push({
            selected: id,
            agent: panel.getAttribute("data-agent"),
            text,
          });
      }).observe(document.body, {
        childList: true,
        subtree: true,
        attributes: true,
      });
    },
    { lead: lead.id, other: other.id },
  );

  // Observe the first animation frame after the real sidebar handler runs.
  // No response can supply the expected text during this interval.
  const switchFrame = async (agent, expected) => {
    const frame = await page.evaluate(
      ({ id }) => {
        const started = performance.now();
        document.querySelector(`[data-chat="${id}"]`).click();
        return new Promise((resolve) =>
          requestAnimationFrame(() => {
            const node = document.querySelector(".agent-panel");
            const content = node?.querySelector(
              ".agent-panel-current, .agent-panel-saved",
            );
            resolve({
              agent: node?.getAttribute("data-agent"),
              text: content?.textContent,
              fit: node?.getAttribute("data-fit"),
              cached: node?.getAttribute("data-cached"),
              height: content?.getBoundingClientRect().height,
              elapsed: performance.now() - started,
            });
          }),
        );
      },
      { id: agent.id },
    );
    assert.equal(
      frame.agent,
      agent.id,
      "First frame belongs to the selected chat",
    );
    assert.equal(
      frame.text,
      expected,
      "Cached progress is visible in the first animation frame",
    );
    assert.equal(frame.fit, "yes");
    assert.equal(
      frame.cached,
      "yes",
      "Held refresh shows an explicit saved copy",
    );
    assert.ok(frame.height > 0);
    diagnostics.push({ switch: agent.id, ...frame });
  };
  hold = true;
  await switchFrame(other, texts.other);
  await switchFrame(lead, texts.lead);
  await until(
    () => held.length > 0,
    "Foreground refresh waits on the held network",
  );
  assert.equal(await currentText().textContent(), texts.lead);
  console.log(
    "PASS prefetched and visited progress render in the first frame with panel HTTP held",
  );

  await writeFile(files.get(lead.id), texts.update);
  const updated = await readPanel(lead);
  await switchFrame(other, texts.other);
  await release();
  await until(
    () =>
      responses.some(
        (response) =>
          response.agent === other.id && response.markdown === texts.other,
      ),
    "Other progress remains available",
  );
  await until(
    () =>
      responses.some(
        (response) =>
          response.agent === lead.id && response.revision === updated.revision,
      ),
    "The background refresh reads the changed file before switching back",
  );
  assert.equal(await panel().getAttribute("data-agent"), other.id);
  assert.equal(
    await currentText().textContent(),
    texts.other,
    "An old chat response cannot replace the current chat",
  );
  await page.locator(`[data-chat="${lead.id}"]`).click();
  await panel()
    .locator(".agent-panel-current")
    .getByText(texts.update, { exact: true })
    .waitFor();
  assert.equal(
    await panel().getAttribute("data-panel-revision"),
    updated.revision,
  );
  assert.equal(await panel().getAttribute("data-cached"), "no");
  console.log("PASS background revision refresh preserves chat isolation");

  assert.deepEqual(
    await page.evaluate(() => window.progressFlashes || []),
    [],
    "No visible progress belongs to another chat",
  );
  hold = true;
  await page.reload();
  await currentText().getByText(texts.update, { exact: true }).waitFor();
  assert.equal(
    await panel().getAttribute("data-panel-revision"),
    updated.revision,
  );
  assert.equal(await panel().getAttribute("data-fit"), "yes");
  await context.setOffline(true);
  await switchFrame(other, texts.other);
  await switchFrame(lead, texts.update);
  await page.screenshot({
    path: join(root, "progress-cached-offline-desktop.png"),
    animations: "disabled",
  });
  console.log(
    "PASS reload restores durable progress; offline chat switches retain measured content",
  );

  await page.setViewportSize({ width: 390, height: 844 });
  await currentText().getByText(texts.update, { exact: true }).waitFor();
  assert.equal(await panel().getAttribute("data-fit"), "yes");
  assert.ok(
    await panel().evaluate(
      (node) =>
        node.scrollWidth <= node.clientWidth + 1 &&
        node.getBoundingClientRect().height <= 151,
    ),
  );
  assert.ok(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth + 1,
    ),
  );
  await page.screenshot({
    path: join(root, "progress-cached-offline-mobile.png"),
    animations: "disabled",
  });
  await context.setOffline(false);
  await release();
  await writeFile(
    files.get(lead.id),
    Array.from({ length: 80 }, (_, index) => `Paragraph ${index}.`).join(
      "\n\n",
    ),
  );
  await panel().getByText("Progress does not fit.", { exact: true }).waitFor();
  assert.equal(await panel().getAttribute("data-fit"), "no");
  assert.equal(
    await currentText().count(),
    0,
    "Cache cannot bypass the layout safety check",
  );
  await writeFile(files.get(lead.id), "");
  await panel().waitFor({ state: "hidden" });
  assert.deepEqual(
    await page.evaluate(() => window.progressFlashes || []),
    [],
    "No visible progress belongs to another chat",
  );
  hold = true;
  await page.reload();
  await page.locator("#message").waitFor();
  assert.equal(
    await panel().count(),
    0,
    "A cached empty file cannot resurrect old progress",
  );
  console.log(
    "PASS cached mobile content fits; oversized and empty revisions retain safety behavior",
  );
  assert.deepEqual(
    await page.evaluate(() => window.progressFlashes || []),
    [],
    "No visible progress belongs to another chat",
  );
  assert.deepEqual(errors, []);
  console.log(
    JSON.stringify({
      browser: browserType.name(),
      evidence: root,
      firstFrames: diagnostics.filter((item) => item.switch),
      panelResponses: responses.length,
    }),
  );
} catch (error) {
  console.error(error);
  console.error(JSON.stringify({ evidence: root, diagnostics }, null, 2));
  await page?.screenshot({ path: join(root, "failure.png") }).catch(() => {});
  throw error;
} finally {
  await page?.unrouteAll({ behavior: "ignoreErrors" });
  await browser?.close();
  if (proc.exitCode === null) {
    const exit = once(proc, "exit");
    proc.kill("SIGTERM");
    await exit;
  }
}
