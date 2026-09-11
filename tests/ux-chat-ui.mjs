// Isolated component fixture. All API requests use local test responses.
import assert from "node:assert/strict";
import { createServer } from "node:http";
import { mkdtemp, readFile, writeFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, dirname, extname } from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
const repo = dirname(dirname(fileURLToPath(import.meta.url)));
const require = createRequire(join(repo, "web/package.json"));
const { chromium } = require("playwright-core");
const { build } = await import(require.resolve("vite"));
const temporary = await mkdtemp(join(tmpdir(), "studio-chat-ux-"));
const entry = join(temporary, "index.html");
await writeFile(
  entry,
  `<html><div id="root"></div><script type="module" src="${join(repo, "tests/fixtures/ux-chat.tsx")}"></script></html>`,
);
let browser;
let server;
try {
  await build({
    configFile: false,
    root: temporary,
    logLevel: "error",
    resolve: {
      alias: [
        { find: /^react$/, replacement: require.resolve("react") },
        {
          find: /^react\/jsx-runtime$/,
          replacement: require.resolve("react/jsx-runtime"),
        },
        {
          find: /^react-dom\/client$/,
          replacement: require.resolve("react-dom/client"),
        },
        {
          find: /^@mantine\/core$/,
          replacement: require.resolve("@mantine/core"),
        },
        {
          find: /^@mantine\/core\/styles.css$/,
          replacement: require.resolve("@mantine/core/styles.css"),
        },
      ],
    },
    build: {
      outDir: join(temporary, "dist"),
      emptyOutDir: true,
      rollupOptions: { input: entry },
    },
  });
  server = createServer(async (request, response) => {
    try {
      const path = new URL(request.url, "http://fixture").pathname;
      const file = join(temporary, "dist", path === "/" ? "index.html" : path);
      response.setHeader(
        "Content-Type",
        { ".html": "text/html", ".js": "text/javascript", ".css": "text/css" }[
          extname(file)
        ] || "text/plain",
      );
      response.end(await readFile(file));
    } catch {
      response.statusCode = 404;
      response.end();
    }
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  browser = await chromium.launch({
    headless: true,
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  });
  const page = await browser.newPage({
    viewport: { width: 1100, height: 800 },
  });
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  page.setDefaultTimeout(10000);
  const records = Array.from({ length: 14 }, (_, index) => ({
    id: `item-${index}`,
    role: index % 2 ? "assistant" : "user",
    at: index + 1,
    turnId: `turn-${Math.floor(index / 2)}`,
    turnStatus: "completed",
    text:
      index === 3
        ? "Unique historical assistant evidence"
        : `Message ${index}. ` + "Readable history. ".repeat(25),
  }));
  records[10].truncated = true;
  records[10].text = "Clipped original prompt";
  let latestItems = records.slice(10);
  let historyVersion = "version-1";
  let failBranch = true;
  const writes = [];
  const pageRequests = [];
  let holdAround = false,
    pendingAround;
  const releaseAround = () => {
    pendingAround?.();
    pendingAround = undefined;
  };
  const snapshot = (id) => ({
    items: id === "lead" ? latestItems : [],
    nextCursor: id === "lead" ? "item-10" : null,
    historyVersion,
    truncated: id === "lead",
    agent: { id, status: "idle" },
  });
  await page.route("**/api/**", async (route) => {
    const request = route.request();
    const url = new URL(request.url());
    const path = url.pathname;
    const id = url.searchParams.get("id");
    if (request.method() === "POST")
      writes.push({ path, body: request.postDataJSON() });
    if (path === "/api/transcript/stream")
      return route.fulfill({
        status: 503,
        json: { error: "Use fixture polling" },
      });
    if (path === "/api/transcript")
      return route.fulfill({ json: snapshot(id) });
    if (path === "/api/transcript/item")
      return route.fulfill({
        json: {
          ...records[10],
          sourceId: "native-user-batch",
          text: "Full original prompt beyond clipped text",
          truncated: false,
        },
      });
    if (path === "/api/transcript/search") {
      const query = url.searchParams.get("q").toLowerCase();
      return route.fulfill({
        json: {
          results: records.filter((record) =>
            record.text.toLowerCase().includes(query),
          ),
        },
      });
    }
    if (path === "/api/transcript/page") {
      pageRequests.push(url.search);
      let items,
        nextCursor,
        nextAfterCursor = null;
      if (url.searchParams.has("around")) {
        if (holdAround)
          await new Promise((resolve) => {
            pendingAround = resolve;
          });
        items = records.slice(2, 6);
        nextCursor = "item-2";
        nextAfterCursor = "item-5";
      } else if (url.searchParams.has("after")) {
        items = records.slice(6, 10);
        nextCursor = "item-6";
        nextAfterCursor = "item-9";
      } else if (url.searchParams.get("before") === "item-10") {
        items = records.slice(6, 10);
        nextCursor = "item-6";
      } else {
        items = records.slice(0, 6);
        nextCursor = null;
      }
      return route.fulfill({
        json: { items, nextCursor, nextAfterCursor, historyVersion },
      });
    }
    if (path === "/api/assets")
      return route.fulfill({
        json: {
          asset: {
            id: "text-asset",
            name: "message.txt",
            mime: "text/plain",
            size: 12050,
            image: false,
          },
        },
      });
    if (path === "/api/branch") {
      if (failBranch) {
        failBranch = false;
        return route.fulfill({
          status: 503,
          json: { error: "Lost branch response" },
        });
      }
      const body = request.postDataJSON();
      return route.fulfill({
        json: {
          id: body.before ? "edited-branch" : "answer-branch",
          draft: {
            text: "Original",
            prefixText: body.before ? "Earlier user instruction" : "",
            assets: [],
          },
        },
      });
    }
    if (path === "/api/queue") return route.fulfill({ json: { items: [] } });
    if (path === "/api/panel")
      return route.fulfill({ json: { html: "", version: 0 } });
    return route.fulfill({ json: { ok: true } });
  });
  await page.goto(`http://127.0.0.1:${server.address().port}`);
  await page.locator('[data-message="item-10"]').waitFor();
  const composer = page.locator("#message");
  await composer.fill("Draft must remain unsent");
  await composer.press("Tab");
  assert.equal(await composer.inputValue(), "Draft must remain unsent");
  assert.equal(
    writes.filter((write) => write.path === "/api/fixture-send").length,
    0,
  );
  assert.equal(
    await composer.evaluate((element) => element === document.activeElement),
    false,
  );
  await page
    .getByRole("button", { name: "Queue after turn", exact: true })
    .click();
  await page.waitForFunction(
    () => document.querySelector("#message").value === "",
  );
  assert.equal(
    writes.find((write) => write.path === "/api/fixture-send").body.delivery,
    "queue",
  );

  const longDraft = "L".repeat(12050);
  await composer.fill(longDraft);
  assert.equal(await composer.inputValue(), longDraft);
  await page.locator("#draft-length-error").waitFor();
  assert.equal(await page.locator("#send").isDisabled(), true);
  await composer.press("Enter");
  assert.equal(
    writes.filter((write) => write.path === "/api/fixture-send").length,
    1,
  );
  await page
    .getByRole("button", { name: "Attach text as a file", exact: true })
    .click();
  await page.waitForFunction(
    () => document.querySelector("#message").value === "",
  );
  assert.equal(
    Buffer.from(
      writes.find((write) => write.path === "/api/assets").body.base64,
      "base64",
    ).toString(),
    longDraft,
  );
  await page
    .getByRole("button", { name: "Remove message.txt", exact: true })
    .click();

  await page.locator("#earlier-messages").click();
  await page.locator('[data-message="item-6"]').waitFor();
  assert.equal(await page.locator('[data-message="item-13"]').count(), 1);
  await page.waitForTimeout(2300);
  assert.equal(
    await page.locator('[data-message="item-6"]').count(),
    1,
    "live updates preserve earlier pages",
  );

  await page
    .getByRole("button", { name: "Browse prompts", exact: true })
    .click();
  const searchDialog = page.getByRole("dialog", { name: "Prompt history" });
  await searchDialog
    .getByRole("textbox", { name: "Search this chat" })
    .fill("Unique historical assistant evidence");
  await searchDialog
    .getByRole("button", {
      name: "Assistant Unique historical assistant evidence",
      exact: true,
    })
    .click();
  await page.locator('[data-message="item-3"][data-found="true"]').waitFor();
  assert.equal(
    await page.locator('[data-message="item-13"]').count(),
    0,
    "distant search shows context without an unmarked history gap",
  );
  const position = await page
    .locator('[data-message="item-3"]')
    .evaluate((element) => {
      const root = document.querySelector("#messages");
      return (
        element.getBoundingClientRect().top - root.getBoundingClientRect().top
      );
    });
  assert.ok(
    position >= 0 && position < 100,
    `exact result is visible: ${position}`,
  );
  await page.locator("#newer-messages").click();
  await page.locator('[data-message="item-9"]').waitFor();
  await page
    .getByRole("button", { name: "Return to latest messages", exact: true })
    .click();
  await page.locator('[data-message="item-13"]').waitFor();
  assert.equal(await page.locator('[data-message="item-3"]').count(), 0);

  // The shell uses the same exact-ID navigation boundary, including repeated targets.
  await page.evaluate(() => window.chatFixture.jump("item-3"));
  await page.locator('[data-message="item-3"][data-found="true"]').waitFor();
  await page.evaluate(() => window.chatFixture.jump("item-3"));
  assert.equal(await page.locator('[data-message="item-3"]').count(), 1);
  await page
    .getByRole("button", { name: "Return to latest messages", exact: true })
    .click();
  await page.locator('[data-message="item-10"]').waitFor();

  // Sending during a distant history read expresses new intent and cancels that view change.
  holdAround = true;
  await page.evaluate(() => window.chatFixture.jump("item-3"));
  for (let attempt = 0; !pendingAround && attempt < 100; attempt++)
    await page.waitForTimeout(20);
  assert.ok(pendingAround, "the historical page read is pending");
  await composer.fill("Send while history loads");
  await page.locator("#send").click();
  await page.waitForFunction(
    () => document.querySelector("#message").value === "",
  );
  holdAround = false;
  releaseAround();
  await page.waitForTimeout(100);
  assert.equal(await page.locator('[data-message="item-13"]').count(), 1);
  assert.equal(await page.locator('[data-message="item-3"]').count(), 0);
  assert.equal(
    await page
      .getByText("This message is unavailable in this conversation.", {
        exact: true,
      })
      .count(),
    0,
  );
  await composer.fill("Source chat draft");
  await page
    .locator('[data-message="item-10"]')
    .getByRole("button", { name: "Edit in a new chat", exact: true })
    .click();
  const edit = page.getByRole("dialog", {
    name: "Edit in a new chat",
    exact: true,
  });
  assert.equal(
    await edit
      .getByRole("textbox", { name: "Draft for the new chat" })
      .inputValue(),
    "Full original prompt beyond clipped text",
  );
  await edit
    .getByRole("textbox", { name: "Draft for the new chat" })
    .fill("Reviewed edited prompt");
  await edit
    .getByRole("button", { name: "Create draft in new chat", exact: true })
    .click();
  await page.getByText("Lost branch response", { exact: true }).waitFor();
  await edit
    .getByRole("button", { name: "Create draft in new chat", exact: true })
    .click();
  await page.waitForFunction(() => window.chatFixture.id === "edited-branch");
  assert.deepEqual(
    await page.evaluate(() => window.chatFixture.branchSelections),
    ["edited-branch"],
  );
  assert.deepEqual(
    await page.evaluate(() => window.chatFixture.regularSelections),
    [],
    "new branch bypasses the stale existing-chat selection guard",
  );
  assert.equal(
    await composer.inputValue(),
    "Earlier user instruction\n\nReviewed edited prompt",
  );
  const attempts = writes.filter((write) => write.path === "/api/branch");
  assert.equal(
    attempts[0].body.id,
    attempts[1].body.id,
    "retry preserves the native branch request identity",
  );
  assert.equal(attempts[0].body.before, true);
  assert.equal(
    attempts[0].body.message_id,
    "item-10",
    "edit preserves the selected input identity inside a native batch",
  );
  assert.equal(
    writes.filter((write) => write.path === "/api/fixture-send").length,
    2,
    "branch creation never sends the reviewed draft",
  );
  await page.evaluate(() => window.chatFixture.select("lead"));
  await page.locator('[data-message="item-13"]').waitFor();
  assert.equal(await composer.inputValue(), "Source chat draft");
  await page
    .locator('[data-message="item-13"]')
    .getByRole("button", { name: "Another answer in a new chat", exact: true })
    .click();
  const again = page.getByRole("dialog", {
    name: "Another answer in a new chat",
    exact: true,
  });
  await again
    .getByRole("button", { name: "Create draft in new chat", exact: true })
    .click();
  await page.waitForFunction(() => window.chatFixture.id === "answer-branch");
  assert.deepEqual(
    await page.evaluate(() => window.chatFixture.branchSelections),
    ["edited-branch", "answer-branch"],
  );
  assert.match(await composer.inputValue(), /Use the existing results/);
  assert.equal(
    writes.filter((write) => write.path === "/api/fixture-send").length,
    2,
  );
  assert.equal(
    writes.filter((write) => write.path === "/api/branch").at(-1).body.before,
    undefined,
  );

  await page.evaluate(() => window.chatFixture.select("lead"));
  await page.locator('[data-message="item-10"]').waitFor();
  await page.locator("#earlier-messages").click();
  await page.locator('[data-message="item-6"]').waitFor();
  historyVersion = "restored-version";
  latestItems = records.slice(12);
  await page.waitForTimeout(2300);
  assert.equal(
    await page.locator('[data-message="item-6"]').count(),
    0,
    "a restore invalidates cached history pages",
  );
  await page.setViewportSize({ width: 390, height: 844 });
  await composer.fill(longDraft);
  assert.ok(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth + 1,
    ),
  );
  assert.equal(await composer.inputValue(), longDraft);
  assert.ok(pageRequests.some((query) => query.includes("around=item-3")));
  assert.deepEqual(errors, []);
  console.log(
    `PASS: chat queue, long draft, complete history, exact search, branch draft and restore boundaries. ${temporary}`,
  );
} finally {
  await browser?.close();
  if (server) await new Promise((resolve) => server.close(resolve));
  await rm(temporary, { recursive: true, force: true });
}
