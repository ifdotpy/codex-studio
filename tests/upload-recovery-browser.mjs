// Real File blobs, IndexedDB transactions, recovery hooks, and composer input.
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
const repo = dirname(dirname(fileURLToPath(import.meta.url)));
const require = createRequire(join(repo, "web/package.json"));
const { chromium, webkit } = require("playwright-core");
const browserType = process.env.BROWSER === "webkit" ? webkit : chromium;
const { createServer } = await import(require.resolve("vite"));
const cache = await mkdtemp(join(tmpdir(), "studio-upload-recovery-"));
const server = await createServer({
  configFile: false,
  root: join(repo, "web"),
  cacheDir: cache,
  optimizeDeps: { include: ["react", "react-dom/client"] },
  server: { host: "127.0.0.1", port: 0, hmr: false },
  plugins: [
    {
      name: "upload-fixture",
      resolveId(id) {
        if (id === "virtual:upload-fixture") return "\0" + id;
      },
      load(id) {
        if (id !== "\0virtual:upload-fixture") return;
        return `
import React from "react";
import {createRoot} from "react-dom/client";
import {MantineProvider} from "@mantine/core";
import "@mantine/core/styles.css";
import ComposerAttachments from "/src/components/ComposerAttachments.tsx";
import {useAttachmentDrafts} from "/src/components/useAttachmentDrafts.ts";
import {useUploadRecovery} from "/src/components/useUploadRecovery.ts";
import {useFormDraft} from "/src/components/useFormDraft.ts";
import {queueUploads,pendingUploads} from "/src/sync/uploads.ts";
export function mount(){createRoot(document.body).render(React.createElement(function Harness(){
  window.notices ||= [];
  const notify = message => window.notices.push(message);
  const [scope,setScope] = React.useState(window.scope);
  const [attachments, setAttachments] = useAttachmentDrafts("codex-agent-attachments:fixture", notify);
  const recovery = useUploadRecovery("fixture", scope, setAttachments);
  const [profile,setProfile] = useFormDraft("fixture-profile:"+scope,notify);
  const [rule,setRule] = useFormDraft("fixture-rule:"+scope,notify);
  window.fixture={attachments,setAttachments,scope,setScope,recovery,profile,setProfile,rule,setRule,pending:()=>pendingUploads("fixture")};
  return React.createElement(MantineProvider,null,React.createElement(ComposerAttachments,{notify,assets:attachments.lead||[],disabled:false,uploading:false,add:files=>queueUploads("fixture",scope=== "b".repeat(32) ? "other" : "lead",files,scope).then(()=>{}),remove:()=>{}}));
}));}
export function mountWriter(){createRoot(document.body).render(React.createElement(function Writer(){const [attachments,setAttachments]=useAttachmentDrafts("codex-agent-attachments:fixture",()=>{});window.fixture={attachments,setAttachments};return null;}));}`;
      },
    },
  ],
});
let browser;
try {
  await server.listen();
  browser = await browserType.launch({
    headless: true,
    executablePath:
      browserType === chromium
        ? process.env.CHROME_BIN ||
          "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
        : undefined,
  });
  const context = await browser.newContext();
  const page = await context.newPage();
  let stage = "initial";
  const errors = [];
  page.on("pageerror", (error) =>
    errors.push({ stage, message: error.message, stack: error.stack }),
  );
  let workspaceId = "a".repeat(32),
    mode = "hold";
  const posts = [],
    headers = [],
    held = [],
    receipts = new Map();
  await page.route("**/check", (route) =>
    route.fulfill({
      contentType: "text/html",
      body: "<!doctype html><title>Recovery</title>",
    }),
  );
  await page.route("**/api/sync/identity", (route) =>
    route.fulfill({ json: { workspaceId } }),
  );
  await page.route("**/api/session", (route) =>
    route.fulfill({ json: { token: "fixture" } }),
  );
  await page.route("**/api/assets", (route) => {
    const body = route.request().postDataJSON();
    posts.push(body);
    headers.push(route.request().headers()["x-canvas-workspace"]);
    if (body.name === "reject.txt")
      return route.fulfill({ status: 400, json: { error: "Rejected file" } });
    const receipt = receipts.get(body.id) || {
      id: body.id,
      name: body.name,
      size: Buffer.from(body.base64, "base64").length,
      mime: "text/plain",
      image: false,
    };
    receipts.set(body.id, receipt);
    if (mode === "hold") {
      held.push({ route, receipt });
      return;
    }
    return route.fulfill({ json: receipt });
  });
  const mount = async () => {
    stage = `mount ${workspaceId}`;
    await page.goto(server.resolvedUrls.local[0] + "check");
    await page.evaluate(async (scope) => {
      window.scope = scope;
      (await import("/@id/virtual:upload-fixture")).mount();
    }, workspaceId);
    await page.waitForFunction(() => !!window.fixture);
  };
  const wait = async (check) => {
    const end = Date.now() + 15000;
    while (!(await check())) {
      if (Date.now() > end)
        throw new Error(
          "Recovery state did not arrive: " +
            JSON.stringify(
              await page.evaluate(() => ({
                notices: window.notices,
                error: window.fixture?.recovery.error,
                pending: window.fixture?.recovery.pending.map((row) => ({
                  id: row.id,
                  name: row.name,
                  size: row.size,
                })),
              })),
            ),
        );
      await new Promise((resolve) => setTimeout(resolve, 20));
    }
  };
  await mount();
  // A failed local commit must leave the selected File objects in the input.
  await page.evaluate(() => {
    const original = IDBDatabase.prototype.transaction;
    window.restoreTransactions = () =>
      (IDBDatabase.prototype.transaction = original);
    IDBDatabase.prototype.transaction = function (...args) {
      if (this.name === "codex-studio-uploads" && args[1] === "readwrite")
        throw new DOMException("Upload storage is full", "QuotaExceededError");
      return original.apply(this, args);
    };
  });
  const input = page.getByLabel("Choose attachments");
  await input.setInputFiles({
    name: "quota.txt",
    mimeType: "text/plain",
    buffer: Buffer.from("Keep selected bytes"),
  });
  await page.waitForFunction(() =>
    window.notices.some((message) =>
      message.includes("Upload storage is full"),
    ),
  );
  assert.equal(await input.evaluate((input) => input.files.length), 1);
  assert.equal(posts.length, 0);
  await page.evaluate(() => window.restoreTransactions());
  await page.evaluate(() => window.fixture.setScope("b".repeat(32)));
  await page.waitForFunction(() => window.fixture.scope === "b".repeat(32));
  await page
    .getByRole("button", { name: "Retry saving files", exact: true })
    .click();
  await wait(() =>
    page.evaluate(async () => (await window.fixture.pending()).length === 1),
  );
  const retryOwner = await page.evaluate(async () => {
    const row = (await window.fixture.pending())[0];
    return { id: row.id, agent: row.agent, workspace: row.workspace };
  });
  assert.equal(retryOwner.agent, "lead");
  assert.equal(
    retryOwner.workspace,
    "a".repeat(32),
    "Failed-save Retry retains the original owner and workspace",
  );
  assert.equal(posts.length, 0);
  await page.evaluate(
    (id) => window.fixture.recovery.remove(id),
    retryOwner.id,
  );
  await page.evaluate(() => window.fixture.setScope("a".repeat(32)));
  await page.waitForFunction(() => window.fixture.scope === "a".repeat(32));
  await input.setInputFiles([
    {
      name: "one.txt",
      mimeType: "text/plain",
      buffer: Buffer.from("First saved bytes"),
    },
    {
      name: "two.txt",
      mimeType: "text/plain",
      buffer: Buffer.from("Second saved bytes"),
    },
  ]);
  await wait(() => posts.length === 1);
  const stored = await page.evaluate(async () =>
    Promise.all(
      (await window.fixture.pending()).map(async (row) => ({
        id: row.id,
        text: await new Promise((resolve, reject) => {
          const open = indexedDB.open("codex-studio-uploads");
          open.onsuccess = () => {
            const db = open.result,
              request = db
                .transaction("contents")
                .objectStore("contents")
                .get(row.id);
            request.onsuccess = () => {
              db.close();
              resolve(new TextDecoder().decode(request.result.bytes));
            };
            request.onerror = () => reject(request.error);
          };
        }),
      })),
    ),
  );
  assert.equal(
    stored.length,
    2,
    "All selected bytes commit before the first upload starts",
  );
  assert.deepEqual(
    stored.map((row) => row.text),
    ["First saved bytes", "Second saved bytes"],
  );
  assert.equal(await input.evaluate((input) => input.files.length), 0);
  await page.evaluate(() => {
    window.fixture.setProfile({
      id: "profile",
      name: "Profile draft",
      instructions: "Keep these instructions",
    });
    window.fixture.setRule({
      id: "rule",
      name: "Rule draft",
      command: "printf recovery",
    });
  });
  // The server committed the first UUID, but this renderer never gets the reply.
  await held.shift().route.abort();
  workspaceId = "b".repeat(32);
  await mount();
  await page.waitForTimeout(200);
  assert.equal(
    posts.length,
    1,
    "A different workspace must not upload these files",
  );
  assert.equal(await page.evaluate(() => window.fixture.profile), null);
  workspaceId = "a".repeat(32);
  mode = "ready";
  await mount();
  await wait(() =>
    page.evaluate(() => window.fixture.attachments.lead?.length === 2),
  );
  assert.deepEqual(
    posts[1],
    posts[0],
    "Lost response retry keeps UUID, owner, name and exact bytes",
  );
  assert.equal(
    receipts.size,
    2,
    "Server sees two assets, despite three attempts",
  );
  assert.equal(
    await page.evaluate(async () => (await window.fixture.pending()).length),
    0,
  );
  assert.equal(
    await page.evaluate(() => window.fixture.profile.instructions),
    "Keep these instructions",
  );
  assert.equal(
    await page.evaluate(() => window.fixture.rule.command),
    "printf recovery",
  );
  mode = "hold";
  await input.setInputFiles({
    name: "cancel.txt",
    mimeType: "text/plain",
    buffer: Buffer.from("Cancel this"),
  });
  await wait(() => held.length === 1);
  const cancelled = posts.at(-1).id;
  await page.evaluate(async (id) => {
    const original = IDBDatabase.prototype.transaction;
    IDBDatabase.prototype.transaction = function (...args) {
      if (this.name === "codex-studio-uploads" && args[1] === "readwrite")
        throw new Error("Deletion interrupted");
      return original.apply(this, args);
    };
    try {
      await window.fixture.recovery.remove(id);
    } catch (error) {
      window.cancelError = error.message;
    }
    IDBDatabase.prototype.transaction = original;
  }, cancelled);
  assert.equal(
    await page.evaluate(() => window.cancelError),
    "Deletion interrupted",
  );
  await held.shift().route.fulfill({ json: receipts.get(cancelled) });
  await page.waitForTimeout(150);
  assert.equal(
    await page.evaluate(() => window.fixture.attachments.lead.length),
    2,
    "A late upload reply cannot recreate a removed attachment",
  );
  mode = "ready";
  await mount();
  await page.waitForTimeout(200);
  assert.equal(
    await page.evaluate(() => window.fixture.attachments.lead.length),
    2,
    "A persisted tombstone prevents recovery after interrupted deletion and reload",
  );
  assert.equal(posts.filter((body) => body.id === cancelled).length, 1);
  await input.setInputFiles([
    {
      name: "reject.txt",
      mimeType: "text/plain",
      buffer: Buffer.from("Rejected first upload"),
    },
    {
      name: "pass.txt",
      mimeType: "text/plain",
      buffer: Buffer.from("Independent upload"),
    },
  ]);
  await wait(() =>
    page.evaluate(() =>
      window.fixture.attachments.lead.some(
        (asset) => asset.name === "pass.txt",
      ),
    ),
  );
  await page.waitForFunction(() =>
    window.fixture.recovery.error.includes("Rejected file"),
  );
  assert.match(
    await page.evaluate(() => window.fixture.recovery.error),
    /Rejected file/,
  );
  const rejected = await page.evaluate(
    async () =>
      (await window.fixture.pending()).find((row) => row.name === "reject.txt")
        .id,
  );
  await page.evaluate((id) => window.fixture.recovery.remove(id), rejected);
  // Freeze the blob read between identity validation and the HTTP request.
  await page.evaluate(() => {
    const read = FileReader.prototype.readAsDataURL;
    FileReader.prototype.readAsDataURL = function (blob) {
      window.releaseFileRead = () => read.call(this, blob);
      FileReader.prototype.readAsDataURL = read;
    };
  });
  await input.setInputFiles({
    name: "scope.txt",
    mimeType: "text/plain",
    buffer: Buffer.from("Immutable workspace"),
  });
  await page.waitForFunction(() => !!window.releaseFileRead);
  await page.evaluate(async () => {
    (await import("/src/api.ts")).setWorkspace("b".repeat(32));
    window.releaseFileRead();
  });
  await wait(() =>
    page.evaluate(() =>
      window.fixture.attachments.lead.some(
        (asset) => asset.name === "scope.txt",
      ),
    ),
  );
  assert.equal(
    headers.at(-1),
    "a".repeat(32),
    "Upload uses its original workspace header after an asynchronous file read",
  );
  const refused = await page.evaluate(async () => {
    try {
      await (
        await import("/src/sync/uploads.ts")
      ).queueUploads(
        "fixture",
        "lead",
        [new File(["old workspace file"], "old.txt")],
        "b".repeat(32),
      );
      return "accepted";
    } catch (error) {
      return error.message;
    }
  });
  assert.match(refused, /workspace changed/i);
  stage = "cross-tab write";
  // Both tabs start with the same attachment map, then add different files.
  const other = await page.context().newPage();
  await other.route("**/check", (route) =>
    route.fulfill({
      contentType: "text/html",
      body: "<!doctype html><title>Other tab</title>",
    }),
  );
  await other.route("**/api/sync/identity", (route) =>
    route.fulfill({ json: { workspaceId } }),
  );
  await other.goto(server.resolvedUrls.local[0] + "check");
  await other.evaluate(async () => {
    (await import("/@id/virtual:upload-fixture")).mountWriter();
  });
  await other.waitForFunction(() => !!window.fixture);
  const write = (page, id) =>
    page.evaluate(
      (id) =>
        window.fixture.setAttachments((current) => ({
          ...current,
          lead: [
            ...(current.lead || []),
            { id, name: id, mime: "text/plain", image: false, size: 1 },
          ],
        })),
      id,
    );
  assert.deepEqual(
    await Promise.all([write(page, "tab-a"), write(other, "tab-b")]),
    [true, true],
  );
  for (const tab of [page, other])
    await tab.waitForFunction(() =>
      ["tab-a", "tab-b"].every((id) =>
        window.fixture.attachments.lead.some((asset) => asset.id === id),
      ),
    );
  const shared = await page.evaluate(() =>
    JSON.parse(
      localStorage.getItem("codex-agent-attachments:fixture"),
    ).lead.map((asset) => asset.id),
  );
  assert.ok(shared.includes("tab-a") && shared.includes("tab-b"));
  await other.close();
  assert.deepEqual(errors, []);
  console.log(
    "PASS: quota failure retains file input, batch blobs commit before HTTP, reload retries exact UUID/content, workspace isolation, durable cancellation across interrupted deletion and reload, failed upload isolation, immutable workspace header, failed-save owner retention, cross-tab attachment writes, profile/rule draft reload",
  );
} finally {
  await browser?.close();
  await server.close();
  await rm(cache, { recursive: true, force: true });
}
