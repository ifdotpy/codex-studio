#!/usr/bin/env node
// Real panel persistence, native preflight, production React, and callback HTTP.
// The fixture uses a temporary database and fake model server.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp, readFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
import { createInterface } from "node:readline";
import { randomUUID } from "node:crypto";
import { request as httpRequest } from "node:http";

const repo = dirname(dirname(fileURLToPath(import.meta.url)));
const { chromium } = createRequire(join(repo, "web/package.json"))(
  "playwright-core",
);
const measureLayout = createRequire(import.meta.url)(
  "../desktop/panel-layout.cjs",
);
const root = await mkdtemp(join(tmpdir(), "studio-structured-panel-"));
const fixture = spawn(
  "python3",
  ["-B", join(repo, "tests/simple-ui-fixture.py"), root],
  {
    stdio: ["pipe", "pipe", "pipe"],
  },
);
let browser,
  page,
  log = "";
const pending = new Map();
fixture.stderr.on("data", (data) => (log += data));
const lines = createInterface({ input: fixture.stdout });
let resolvePort, rejectPort;
const portReady = new Promise((resolve, reject) => {
  resolvePort = resolve;
  rejectPort = reject;
});
lines.on("line", (line) => {
  if (/^\d+$/.test(line)) {
    resolvePort(Number(line));
    return;
  }
  try {
    const reply = JSON.parse(line);
    pending.get(reply.id)?.(reply);
    pending.delete(reply.id);
  } catch {
    log += line + "\n";
  }
});
fixture.once("exit", () =>
  rejectPort(new Error(log || "Fixture exited before publishing its port")),
);
const callPanel = (agent, params) =>
  new Promise((resolve, reject) => {
    const id = randomUUID();
    const timeout = setTimeout(() => {
      pending.delete(id);
      reject(new Error("Panel fixture timed out: " + log));
    }, 60000);
    pending.set(id, (reply) => {
      clearTimeout(timeout);
      resolve(reply);
    });
    fixture.stdin.write(
      JSON.stringify({
        method: "fixture/panel-action",
        id,
        agent: agent.id,
        params,
      }) + "\n",
    );
  });
const poll = async (fn, label) => {
  for (let n = 0; n < 150; n++) {
    if (await fn()) return;
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  throw new Error(label + "\n" + log);
};
try {
  const port = await portReady;
  const origin = `http://127.0.0.1:${port}`;
  const state = await (await fetch(origin + "/api/state")).json();
  for (const asset of ["/assets/panel-bridge.js", "/assets/panel-ui.js"]) {
    assert.equal(
      (await fetch(origin + asset, { headers: { Origin: "null" } })).status,
      200,
    );
    const rejectedHost = await new Promise((resolve, reject) => {
      const request = httpRequest(
        origin + asset,
        { headers: { Origin: "null", Host: "outside.invalid" } },
        (response) => {
          response.resume();
          resolve(response.statusCode);
        },
      );
      request.on("error", reject);
      request.end();
    });
    assert.equal(rejectedHost, 403);
  }
  assert.equal(
    (await fetch(origin + "/api/state", { headers: { Origin: "null" } }))
      .status,
    403,
    "The opaque panel cannot read the workspace API",
  );
  const lead = state.threads.find((agent) => agent.name === "Release lead");
  const other = state.threads.find((agent) => agent.name === "Other project");
  const readPanel = async (agent) =>
    (await fetch(`${origin}/api/panel?agent=${agent.id}`)).json();
  const publish = async (agent, content) => {
    const reply = await callPanel(agent, content);
    assert.equal(reply.ok, true, reply.error);
    const stored = await readPanel(agent);
    assert.equal(stored.version, reply.result.version);
    return stored;
  };
  const catalog = await callPanel(lead, { action: "catalog" });
  assert.equal(catalog.ok, true, catalog.error);
  assert.match(JSON.stringify(catalog.result), /json-render/);
  const example = JSON.parse(
    await readFile(
      join(repo, ".agents/skills/codex-workspace/assets/panel-progress.json"),
      "utf8",
    ),
  );
  let accepted = await publish(lead, example);
  assert.equal(accepted.format, "json-render");
  assert.deepEqual(accepted.spec, example.spec);
  assert.equal(accepted.html, "");

  browser = await chromium.launch({
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    headless: true,
  });
  page = await browser.newPage({ viewport: { width: 1280, height: 1000 } });
  const errors = [],
    submissions = [];
  page.on("pageerror", (error) => errors.push(error.message));
  page.on("console", (message) => {
    if (message.type() === "error") log += message.text() + "\n";
  });
  page.on(
    "requestfailed",
    (request) =>
      (log += request.url() + ": " + request.failure()?.errorText + "\n"),
  );
  page.on("request", (request) => {
    if (new URL(request.url()).pathname === "/api/panel/callback")
      submissions.push(request.postDataJSON());
  });
  await page.goto(origin);
  const selectChat = async (name) =>
    page.locator("[data-chat]").filter({ hasText: name }).click();
  await selectChat("Release lead");
  const panel = page.locator(".agent-panel");
  const frame = panel.frameLocator("iframe");
  await frame.getByRole("tab", { name: "Runtime", exact: true }).waitFor();
  await frame.getByRole("progressbar", { name: "Runtime checks" }).waitFor();
  const assertFits = async () => {
    const bounds = await panel.boundingBox();
    assert.equal(bounds.height, 150);
    assert.equal(
      await panel.evaluate((element) => element.nextElementSibling?.id),
      "composer",
    );
    assert.equal((await panel.locator("iframe").boundingBox()).height, 150);
    const layout = await frame.locator("body").evaluate(measureLayout);
    assert.equal(layout.fits, true, JSON.stringify(layout));
  };
  await assertFits();
  assert.equal(
    await frame
      .locator("body")
      .evaluate((element) => getComputedStyle(element).backgroundColor),
    await page.locator("#messages").evaluate((element) => {
      for (let current = element; current; current = current.parentElement) {
        const color = getComputedStyle(current).backgroundColor;
        if (color !== "rgba(0, 0, 0, 0)" && color !== "transparent")
          return color;
      }
      return "transparent";
    }),
    "Panel and transcript share their surface color",
  );
  for (const [tab, label] of [
    ["SDK", "SDK checks"],
    ["Desktop", "Desktop checks"],
    ["Runtime", "Runtime checks"],
  ]) {
    await frame.getByRole("tab", { name: tab, exact: true }).click();
    await frame.getByRole("progressbar", { name: label }).waitFor();
    await assertFits();
  }
  assert.equal(
    submissions.length,
    0,
    "Rendering and local tabs never call the agent",
  );
  await page.screenshot({ path: join(root, "structured-desktop.png") });
  for (const width of [320, 1000]) {
    // The mobile shell intentionally omits the panel. Exercise the panel's
    // narrow geometry without switching this desktop integration into mobile.
    await panel.evaluate((element, width) => {
      element.style.width = `${width}px`;
    }, width);
    for (const [tab, label] of [
      ["SDK", "SDK checks"],
      ["Desktop", "Desktop checks"],
      ["Runtime", "Runtime checks"],
    ]) {
      await frame.getByRole("tab", { name: tab, exact: true }).click();
      await frame.getByRole("progressbar", { name: label }).waitFor();
      await assertFits();
    }
    await page.screenshot({ path: join(root, `structured-${width}.png`) });
  }
  await page.setViewportSize({ width: 1280, height: 1000 });
  const details = frame.getByRole("button", { name: "Details", exact: true });
  // A synthetic DOM event must not pass the trusted-user boundary.
  await details.evaluate((element) => element.click());
  await page.waitForTimeout(150);
  assert.equal(submissions.length, 0);
  await details.click();
  await poll(() => submissions.length === 1, "Explicit button callback");
  await poll(
    async () =>
      (await readPanel(lead)).submittedCallbacks.includes("details_runtime"),
    "Callback stored by the real server",
  );
  assert.equal(submissions[0].agent, lead.id);
  assert.equal(submissions[0].version, accepted.version);
  assert.equal(submissions[0].callback, "details_runtime");
  assert.deepEqual(submissions[0].values, {});
  await poll(() => details.isDisabled(), "Accepted button disabled");
  // Force the persisted RxDB projection to win the race against fresh auth.
  // Its cache must never persist credentials, and later HTTP must still update them.
  let delayAuth = true;
  await page.route("**/api/state", async (route) => {
    if (delayAuth) {
      delayAuth = false;
      await new Promise((resolve) => setTimeout(resolve, 1000));
    }
    await route.continue();
  });
  await page.reload();
  await selectChat("Release lead");
  await frame.getByRole("progressbar", { name: "Runtime checks" }).waitFor();
  assert.equal(
    await details.isDisabled(),
    true,
    "Accepted callback remains disabled after reload",
  );
  assert.equal(submissions.length, 1);

  await selectChat("Other project");
  await panel.waitFor({ state: "hidden" });
  assert.equal((await readPanel(other)).version, 0);
  await selectChat("Release lead");
  await frame.getByRole("progressbar", { name: "Runtime checks" }).waitFor();
  assert.equal(await panel.getAttribute("data-agent"), lead.id);
  assert.equal(submissions.length, 1, "Chat changes do not replay callbacks");

  const malformed = {
    action: "set",
    spec: {
      root: "invalid",
      elements: { invalid: { type: "UnknownComponent", props: {} } },
    },
  };
  const overflow = structuredClone(example);
  overflow.spec.elements.sdk = {
    type: "Text",
    props: { text: "Measured content needs room. ".repeat(17) },
    visible: { $state: "/project", eq: "sdk" },
  };
  for (const id of [
    "sdk-steps",
    "sdk-progress",
    "sdk-footer",
    "sdk-status",
    "sdk-details",
  ])
    delete overflow.spec.elements[id];
  overflow.callbacks = overflow.callbacks.filter(
    (callback) => callback.id !== "details_sdk",
  );
  for (const [label, content, error] of [
    [
      "unknown component",
      malformed,
      /UnknownComponent|unknown component|type/i,
    ],
    ["hidden overflowing tab", overflow, /fit|overflow|150/i],
    ["mixed transport", { ...example, html: "" }, /not both/i],
  ]) {
    const before = await readPanel(lead);
    const rejected = await callPanel(lead, content);
    assert.equal(rejected.ok, false, label + " was accepted");
    assert.match(rejected.error, error);
    assert.deepEqual(
      await readPanel(lead),
      before,
      label + " must preserve previous content and callbacks",
    );
  }
  await assertFits();
  assert.equal(
    submissions.length,
    1,
    "Preflight rejection does not generate a callback",
  );

  const form = {
    action: "set",
    callbacks: [
      { id: "choose", label: "Apply choice", fields: ["scope", "notify"] },
    ],
    spec: {
      root: "form",
      state: { scope: "changed", notify: false },
      elements: {
        form: {
          type: "Form",
          props: { callback: "choose", submitLabel: "Apply" },
          children: ["scope", "notify"],
        },
        scope: {
          type: "Select",
          props: {
            name: "scope",
            label: "Scope",
            value: { $bindState: "/scope" },
            options: [
              { label: "Changed", value: "changed" },
              { label: "All", value: "all" },
            ],
          },
        },
        notify: {
          type: "Toggle",
          props: {
            name: "notify",
            label: "Notify",
            checked: { $bindState: "/notify" },
          },
        },
      },
    },
  };
  accepted = await publish(lead, form);
  await frame.getByRole("combobox", { name: "Scope" }).waitFor();
  await frame.getByRole("combobox", { name: "Scope" }).selectOption("all");
  await frame.getByRole("checkbox", { name: "Notify" }).check();
  assert.equal(
    await frame.getByRole("combobox", { name: "Scope" }).inputValue(),
    "all",
  );
  assert.equal(
    await frame.getByRole("checkbox", { name: "Notify" }).isChecked(),
    true,
  );
  assert.equal(
    submissions.length,
    1,
    "Select and Toggle update only local state",
  );
  assert.deepEqual(
    (await readPanel(lead)).spec.state,
    form.spec.state,
    "Local state does not rewrite the published revision",
  );
  await assertFits();
  await frame.getByRole("button", { name: "Apply", exact: true }).click();
  await poll(() => submissions.length === 2, "Explicit form submit");
  await poll(
    async () => (await readPanel(lead)).submittedCallbacks.includes("choose"),
    "Form callback committed",
  );
  assert.equal(submissions[1].version, accepted.version);
  assert.deepEqual(submissions[1].values, { scope: ["all"], notify: ["true"] });
  assert.equal(
    await frame
      .getByRole("button", { name: "Apply", exact: true })
      .isDisabled(),
    true,
  );
  const repeat = await fetch(origin + "/api/panel/callback", {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      "X-Canvas-Token": state.token,
      Origin: origin,
    },
    body: JSON.stringify({ ...submissions[1], id: randomUUID() }),
  });
  assert.equal(repeat.status, 200);
  const replay = await repeat.json();
  assert.equal(replay.callback, "choose");
  assert.deepEqual((await readPanel(lead)).submittedCallbacks, ["choose"]);
  const changed = await fetch(origin + "/api/panel/callback", {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      "X-Canvas-Token": state.token,
      Origin: origin,
    },
    body: JSON.stringify({
      ...submissions[1],
      id: randomUUID(),
      values: { scope: ["changed"] },
    }),
  });
  assert.equal(
    changed.status,
    409,
    "A callback cannot deliver different values twice in one version",
  );
  await publish(lead, form);
  await poll(
    async () =>
      !(await frame
        .getByRole("button", { name: "Apply", exact: true })
        .isDisabled()),
    "New version unlocks form",
  );
  for (const width of [320, 1000]) {
    // The mobile shell intentionally omits the panel. Exercise the panel's
    // narrow geometry without switching this desktop integration into mobile.
    await panel.evaluate((element, width) => {
      element.style.width = `${width}px`;
    }, width);
    await assertFits();
  }
  assert.equal(submissions.length, 2, "New panel versions do not auto-submit");
  const editable = {
    action: "set",
    spec: {
      root: "layout",
      state: { first: "One", second: "Two" },
      elements: {
        layout: {
          type: "Stack",
          props: { gap: "xs" },
          children: ["inputs", "mirrors"],
        },
        inputs: {
          type: "Grid",
          props: { columns: 2 },
          children: ["first-input", "second-input"],
        },
        mirrors: {
          type: "Grid",
          props: { columns: 2 },
          children: ["first-text", "second-text"],
        },
        "first-input": {
          type: "TextInput",
          props: {
            name: "first",
            label: "First",
            value: { $bindState: "/first" },
          },
        },
        "second-input": {
          type: "TextInput",
          props: {
            name: "second",
            label: "Second",
            value: { $bindState: "/second" },
          },
        },
        "first-text": { type: "Text", props: { text: { $state: "/first" } } },
        "second-text": { type: "Text", props: { text: { $state: "/second" } } },
      },
    },
  };
  await panel.evaluate((element) => {
    element.style.width = "320px";
  });
  await publish(lead, editable);
  await frame.getByRole("textbox", { name: "First", exact: true }).waitFor();
  await assertFits();
  const initial = ["One", "Two"];
  await frame.locator("body").evaluate(() => {
    const inputs = Array.from(document.querySelectorAll("input"));
    const setter = Object.getOwnPropertyDescriptor(
      HTMLInputElement.prototype,
      "value",
    ).set;
    for (const [index, input] of inputs.entries()) {
      setter.call(
        input,
        (index ? "Second long content " : "First long content ").repeat(20),
      );
      input.dispatchEvent(new Event("input", { bubbles: true }));
    }
  });
  await page
    .locator(".agent-panel-feedback")
    .getByText(/previous view was restored/)
    .waitFor();
  await poll(
    async () =>
      JSON.stringify(
        await frame
          .locator("input")
          .evaluateAll((inputs) => inputs.map((input) => input.value)),
      ) === JSON.stringify(initial),
    "Rapid updates restore both fields to the last fitting snapshot",
  );
  assert.deepEqual(await frame.locator(".sp-text").allTextContents(), initial);
  await assertFits();
  assert.equal(
    submissions.length,
    2,
    "Rejected local updates never call the agent",
  );
  assert.deepEqual(errors, []);
  console.log(
    JSON.stringify({
      passed: true,
      realPersistence: true,
      nativePreflight: true,
      tabs: true,
      localFields: true,
      realCallbacks: true,
      oncePerVersion: true,
      rejectedWrites: true,
      chatIsolation: true,
      reload: true,
      widths: [320, 1000],
      screenshots: root,
    }),
  );
} catch (error) {
  await page?.screenshot({ path: join(root, "failure.png") }).catch(() => {});
  console.error("Failure evidence:", root, "\n", log);
  throw error;
} finally {
  await browser?.close();
  fixture.kill("SIGTERM");
  if (fixture.exitCode === null)
    await new Promise((resolve) => fixture.once("exit", resolve));
}
