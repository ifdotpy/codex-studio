// Real AgentPanel, structured iframe and geometry. Only feed HTTP is a fixture.
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { readFile } from "node:fs/promises";
import { resolve } from "node:path";
const require = createRequire(new URL("../web/package.json", import.meta.url));
const { createServer } = await import(require.resolve("vite"));
const { chromium } = require("playwright-core");
const measureLayout = require("../desktop/panel-layout.cjs");
const root = resolve(import.meta.dirname, "../web");
const server = await createServer({
  configFile: false,
  root,
  server: { host: "127.0.0.1", port: 0 },
  plugins: [
    {
      name: "panel-feed-fixture",
      configureServer(server) {
        server.middlewares.use("/feed-test", (_req, res) => {
          res.setHeader("Content-Type", "text/html");
          res.end(
            '<html><body style="margin:0"><div id="root"></div><script type="module" src="/feed-entry.tsx"></script></body></html>',
          );
        });
        server.middlewares.use("/assets/panel-ui.js", async (_req, res) => {
          res.setHeader("Content-Type", "text/javascript");
          res.end(await readFile(resolve(root, "dist/assets/panel-ui.js")));
        });
      },
      resolveId(id) {
        if (id === "/feed-entry.tsx") return root + id;
      },
      load(id) {
        if (id === root + "/feed-entry.tsx")
          return `import React,{useState}from'react';import{createRoot}from'react-dom/client';import AgentPanel from'/src/components/AgentPanel.tsx';import'/src/studio-theme.css';function App(){const[n,set]=useState(0);window.refreshPanel=set;return <AgentPanel agentId="feed-lead" version={1} dataVersion={n} token="fixture"/>}createRoot(document.getElementById('root')).render(<App/>);`;
      },
    },
  ],
});
const panelSnapshot = {
  agent: "feed-lead",
  version: 1,
  dataVersion: 1,
  format: "json-render",
  html: "",
  css: "",
  updated: 1,
  feed: {
    monitorId: "collector",
    statePath: "/live",
    status: "running",
    sequence: 1,
  },
  callbacks: [{ id: "apply", label: "Apply", fields: ["note"] }],
  submittedCallbacks: [],
  spec: {
    root: "tabs",
    state: { localTab: "data", note: "Draft", live: { label: "Instance one" } },
    elements: {
      tabs: {
        type: "Tabs",
        props: {
          value: { $bindState: "/localTab" },
          options: [
            { label: "Instances", value: "data" },
            { label: "Controls", value: "controls" },
          ],
        },
        children: ["data", "form"],
      },
      data: {
        type: "Stack",
        props: { direction: "row" },
        visible: { $state: "/localTab", eq: "data" },
        children: ["icon", "text"],
      },
      icon: {
        type: "Icon",
        props: { name: "server", label: "EC2 instance", tone: "success" },
      },
      text: { type: "Text", props: { text: { $state: "/live/label" } } },
      form: {
        type: "Form",
        props: { callback: "apply", submitLabel: "Apply" },
        visible: { $state: "/localTab", eq: "controls" },
        children: ["note"],
      },
      note: {
        type: "TextInput",
        props: { name: "note", label: "Note", value: { $bindState: "/note" } },
      },
    },
  },
};
let snapshot = { ...structuredClone(panelSnapshot), dataVersion: 0 };
delete snapshot.feed;
let holdNext = false,
  heldResponse;

const submissions = [];
await server.listen();
const browser = await chromium.launch({
  executablePath:
    process.env.CHROME_BIN ||
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  headless: true,
});
try {
  const page = await browser.newPage({ viewport: { width: 640, height: 600 } });
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.route("**/api/panel?**", async (route) => {
    const body = structuredClone(snapshot);
    if (holdNext) {
      holdNext = false;
      await new Promise((resolve) => {
        heldResponse = resolve;
      });
    }
    await route.fulfill({ json: body }).catch(() => {});
  });
  await page.route("**/api/panel/callback", (route) => {
    const body = route.request().postDataJSON();
    submissions.push(body);
    snapshot.submittedCallbacks.push(body.callback);
    return route.fulfill({ json: { ...body, status: "queued" } });
  });
  await page.goto(server.resolvedUrls.local[0] + "feed-test");
  const panel = page.locator(".agent-panel"),
    frame = panel.frameLocator("iframe");
  await frame.getByText("Instance one", { exact: true }).waitFor();
  await frame.getByRole("img", { name: "EC2 instance" }).waitFor();
  // Record the actual browsing context and channel, not just the DOM iframe element.
  const identity = await frame.locator("body").evaluate(() => {
    window.feedIdentity = crypto.randomUUID();
    const config = JSON.parse(
      decodeURIComponent(
        document
          .querySelector("script[data-config]")
          .getAttribute("data-config"),
      ),
    );
    return { marker: window.feedIdentity, channel: config.channel };
  });
  const assertIdentity = async () =>
    assert.deepEqual(
      await frame.locator("body").evaluate(() => ({
        marker: window.feedIdentity,
        channel: JSON.parse(
          decodeURIComponent(
            document
              .querySelector("script[data-config]")
              .getAttribute("data-config"),
          ),
        ).channel,
      })),
      identity,
    );
  const update = async (label, status = "running", error) => {
    snapshot = {
      ...snapshot,
      dataVersion: snapshot.dataVersion + 1,
      feed: {
        ...panelSnapshot.feed,
        ...snapshot.feed,
        status,
        error,
        sequence: (snapshot.feed?.sequence || 0) + 1,
      },
      spec: {
        ...snapshot.spec,
        state: { ...snapshot.spec.state, live: { label } },
      },
    };
    await page.evaluate(
      (version) => window.refreshPanel(version),
      snapshot.dataVersion,
    );
    await page.waitForFunction(
      (version) =>
        document.querySelector(".agent-panel")?.dataset.panelDataVersion ===
        String(version),
      snapshot.dataVersion,
    );
  };
  await frame.getByRole("tab", { name: "Controls" }).click();
  await frame.getByRole("textbox", { name: "Note" }).fill("Keep my draft");
  // Attach a feed to an existing template with a status-only update first.
  await update("Instance one", "starting");
  await assertIdentity();
  assert.equal(
    await frame.getByRole("textbox", { name: "Note" }).inputValue(),
    "Keep my draft",
  );
  await update("Instance two");
  await assertIdentity();
  assert.equal(
    await frame
      .getByRole("tab", { name: "Controls" })
      .getAttribute("aria-selected"),
    "true",
  );
  assert.equal(
    await frame.getByRole("textbox", { name: "Note" }).inputValue(),
    "Keep my draft",
  );
  assert.equal(submissions.length, 0, "Feed refresh does not call the agent");
  await frame.getByRole("tab", { name: "Instances" }).click();
  await frame.getByText("Instance two", { exact: true }).waitFor();
  await update("Instance three");
  await frame.getByText("Instance three", { exact: true }).waitFor();
  await assertIdentity();
  await frame.getByRole("tab", { name: "Controls" }).click();
  await frame.getByRole("button", { name: "Apply", exact: true }).click();
  await page.waitForFunction(() =>
    document
      .querySelector(".agent-panel-feedback")
      ?.textContent.includes("sent to agent"),
  );
  assert.equal(submissions.length, 1);
  assert.equal(submissions[0].version, 1);
  assert.deepEqual(submissions[0].values, { note: ["Keep my draft"] });
  await update("Instance four");
  assert.equal(
    await frame
      .getByRole("button", { name: "Apply", exact: true })
      .isDisabled(),
    true,
  );
  await frame.getByRole("tab", { name: "Instances" }).click();
  await frame.getByText("Instance four", { exact: true }).waitFor();
  await page.getByRole("button", { name: "Dismiss panel notice" }).click();
  await update("Instance four", "stale");
  await page.getByText("Live data unavailable", { exact: true }).waitFor();
  await frame.getByText("Instance four", { exact: true }).waitFor();
  await assertIdentity();
  assert.equal((await panel.boundingBox()).height, 150);
  // A trusted-host update still cannot replace a separate local state subtree.
  // Invalid values and geometry reject atomically, preserving the confirmed snapshot.
  await page.setViewportSize({ width: 320, height: 600 });
  await update("Long infrastructure label ".repeat(18));
  await page.getByText(/previous view was restored/).waitFor();
  await frame.getByText("Instance four", { exact: true }).waitFor();
  assert.equal(
    (await frame.locator("body").evaluate(measureLayout)).fits,
    true,
  );
  assert.equal((await panel.boundingBox()).height, 150);
  await update("Instance five");
  await frame.getByText("Instance five", { exact: true }).waitFor();
  // Foreign documents and obsolete snapshots cannot write into the live view.
  await page.evaluate(({ channel }) => {
    const target = document.querySelector("iframe").contentWindow;
    target.postMessage(
      {
        type: "panel-data",
        channel: "wrong",
        dataVersion: 999,
        statePath: "/live",
        value: { label: "Wrong channel" },
      },
      "*",
    );
    target.postMessage(
      {
        type: "panel-data",
        channel,
        dataVersion: 1,
        statePath: "/live",
        value: { label: "Old snapshot" },
      },
      "*",
    );
  }, identity);
  await page.waitForTimeout(80);
  await frame.getByText("Instance five", { exact: true }).waitFor();
  await assertIdentity();
  assert.equal(
    submissions.length,
    1,
    "Feed and rejected updates never replay a callback",
  );
  holdNext = true;
  snapshot = { ...snapshot, dataVersion: snapshot.dataVersion + 1 };
  await page.evaluate(
    (version) => window.refreshPanel(version),
    snapshot.dataVersion,
  );
  for (let i = 0; !heldResponse && i < 100; i++) await page.waitForTimeout(10);
  assert.ok(heldResponse, "The old snapshot request is pending");
  await update("Newest instance");
  await frame.getByText("Newest instance", { exact: true }).waitFor();
  heldResponse();
  await page.waitForTimeout(80);
  await frame.getByText("Newest instance", { exact: true }).waitFor();
  await assertIdentity();
  await update("Newest instance", "running", "Collector frame was rejected");
  await page.getByText("Live data unavailable", { exact: true }).waitFor();
  await frame.getByText("Newest instance", { exact: true }).waitFor();
  assert.equal(submissions.length, 1);
  await frame.locator("body").evaluate(() => {
    const config = JSON.parse(
      decodeURIComponent(
        document
          .querySelector("script[data-config]")
          .getAttribute("data-config"),
      ),
    );
    parent.postMessage(
      {
        type: "panel-error",
        channel: config.channel,
        error: "Temporary renderer failure",
      },
      "*",
    );
  });
  await page.getByRole("button", { name: "Retry", exact: true }).click();
  await frame.getByText("Newest instance", { exact: true }).waitFor();
  assert.equal(
    await frame.locator("body").evaluate(() => window.feedIdentity),
    undefined,
    "Explicit retry can restart a failed frame",
  );
  assert.equal(
    submissions.length,
    1,
    "Explicit retry does not replay a callback",
  );
  assert.deepEqual(errors, []);
  console.log(
    JSON.stringify({
      passed: true,
      stableIframe: true,
      localState: true,
      icons: true,
      callbacksOnce: true,
      rollback: true,
      height: 150,
    }),
  );
} finally {
  await browser.close();
  await server.close();
}
