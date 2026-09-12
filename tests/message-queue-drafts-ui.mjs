#!/usr/bin/env node
// Actual queue component, isolated browser storage, no backend or model service.
import assert from "node:assert/strict";
import { createServer } from "node:http";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createRequire } from "node:module";
import { pathToFileURL } from "node:url";

const repo = join(import.meta.dirname, "..");
const require = createRequire(join(repo, "web/package.json"));
const { chromium, webkit } = require("playwright-core");
const browserType = process.env.BROWSER === "webkit" ? webkit : chromium;
const { build } = await import(pathToFileURL(require.resolve("vite")));
const root = await mkdtemp(join(tmpdir(), "studio-message-queue-drafts-"));
const entry = join(root, "entry.tsx");
let browser;
let server;
let passed = false;
const errors = [];
try {
  await writeFile(
    entry,
    `
import { useState } from ${JSON.stringify(require.resolve("react"))};
import { createRoot } from ${JSON.stringify(require.resolve("react-dom/client"))};
import MessageQueue from ${JSON.stringify(join(repo, "web/src/components/MessageQueue.tsx"))};
function App() {
  const [items, setItems] = useState([{ id: "q1", text: "Original message" }]);
  return <MessageQueue scope="journal-fixture" items={items} canReorder={true}
    onEdit={async (item, text) => setItems([{ ...item, text }])}
    onCancel={async () => setItems([])} onReorder={async () => {}} />;
}
createRoot(document.getElementById("root")!).render(<App />);
`,
  );
  await build({
    configFile: false,
    root,
    logLevel: "error",
    resolve: {
      alias: { "react/jsx-runtime": require.resolve("react/jsx-runtime") },
    },
    define: { "process.env.NODE_ENV": JSON.stringify("test") },
    build: {
      outDir: root,
      emptyOutDir: false,
      lib: {
        entry,
        formats: ["iife"],
        name: "QueueDraftTest",
        fileName: () => "app.js",
        cssFileName: "app",
      },
      minify: false,
    },
  });
  const files = {
    "/app.js": ["text/javascript", await readFile(join(root, "app.js"))],
    "/app.css": ["text/css", await readFile(join(root, "app.css"))],
    "/": [
      "text/html",
      '<link rel="stylesheet" href="/app.css"><div id="root"></div><script src="/app.js"></script>',
    ],
  };
  server = createServer((request, response) => {
    const file = files[request.url];
    response.writeHead(file ? 200 : 404, {
      "Content-Type": file?.[0] || "text/plain",
    });
    response.end(file?.[1] || "Not found");
  });
  await new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  const url = `http://127.0.0.1:${server.address().port}`;
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
  const observe = (context) => {
    context.setDefaultTimeout(10000);
    context.on("page", (page) =>
      page.on("pageerror", (error) => errors.push(error.message)),
    );
    return context;
  };
  const quota = observe(await browser.newContext());
  await quota.addInitScript(() => {
    const original = Storage.prototype.setItem;
    Storage.prototype.setItem = function (key, value) {
      if (key.startsWith("codex-queue-edit:"))
        throw new DOMException("Full", "QuotaExceededError");
      return original.call(this, key, value);
    };
  });
  const page = await quota.newPage();
  await page.goto(url);
  await page
    .getByRole("button", { name: "Edit queued message 1", exact: true })
    .click();
  await page
    .getByRole("textbox", { name: "Edit queued message", exact: true })
    .fill("Keep this text");
  await page
    .getByRole("alert")
    .filter({ hasText: "could not be saved" })
    .waitFor();
  await page.getByText("Edit not saved", { exact: true }).waitFor();
  await page.getByRole("button", { name: "Cancel edit", exact: true }).click();
  assert.equal(await page.getByRole("textbox").count(), 0);
  console.log("PASS quota failure is visible and Cancel closes editor");
  await quota.close();
  const context = observe(await browser.newContext());
  const first = await context.newPage();
  await first.goto(url);
  await first
    .getByRole("button", { name: "Edit queued message 1", exact: true })
    .click();
  await first.getByRole("textbox").fill("Before reload");
  await first.reload();
  assert.equal(await first.getByRole("textbox").inputValue(), "Before reload");
  await first.getByRole("textbox").fill("After reload");
  await first
    .getByRole("button", { name: "Save queued message", exact: true })
    .click();
  await first
    .getByRole("button", { name: "Edit queued message 1", exact: true })
    .waitFor();
  assert.equal(
    await first.evaluate(
      () =>
        Object.keys(localStorage).filter((k) =>
          k.startsWith("codex-queue-edit:"),
        ).length,
    ),
    0,
  );
  await first.reload();
  assert.equal(await first.getByRole("textbox").count(), 0);
  console.log("PASS Save removes unchanged recovery ancestors");
  await first
    .getByRole("button", { name: "Edit queued message 1", exact: true })
    .click();
  await first.getByRole("textbox").fill("Tab A original");
  const second = await context.newPage();
  await second.goto(url);
  assert.equal(
    await second.getByRole("textbox").inputValue(),
    "Tab A original",
  );
  await second.getByRole("textbox").fill("Tab B edit");
  await first.getByRole("textbox").fill("Tab A concurrent edit");
  await second
    .getByRole("button", { name: "Save queued message", exact: true })
    .click();
  await second
    .getByRole("button", { name: "Edit queued message 1", exact: true })
    .waitFor();
  const records = await second.evaluate(() =>
    Object.keys(localStorage)
      .filter((k) => k.startsWith("codex-queue-edit:"))
      .map((k) => JSON.parse(localStorage.getItem(k))),
  );
  assert.deepEqual(
    records.map((r) => r.text),
    ["Tab A concurrent edit"],
  );
  await second.reload();
  assert.equal(
    await second.getByRole("textbox").inputValue(),
    "Tab A concurrent edit",
  );
  console.log("PASS concurrent tab edit survives another tab Save and reload");
  await context.close();
  assert.deepEqual(errors, [], "The component reports no browser errors");
  console.log(JSON.stringify({ browser: browserType.name(), checks: 3 }));
  passed = true;
} catch (error) {
  for (const [index, page] of (
    browser?.contexts().flatMap((context) => context.pages()) || []
  ).entries()) {
    await page
      .screenshot({ path: join(root, `failure-${index}.png`) })
      .catch(() => {});
  }
  console.error(`Failure evidence: ${root}`);
  throw error;
} finally {
  await browser?.close();
  if (server?.listening) await new Promise((resolve) => server.close(resolve));
  if (passed && !process.env.KEEP_TEST_ARTIFACTS)
    await rm(root, { recursive: true, force: true });
  else if (passed) console.log(`Evidence: ${root}`);
}
