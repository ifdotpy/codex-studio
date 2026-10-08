// Isolated component fixture. All API requests use local test responses.
import { createServer } from "node:http";
import { mkdtemp, writeFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import {
  API_SCHEMA_HASH_HEADER,
  apiSchemaHandshakeSse,
  readApiSchemaHash,
  syncProtocolFixture,
  test,
  expect,
} from "../playwright.mjs";

test("Ux Chat Ui", async ({
  browser: _testBrowser,
  context: _testContext,
  page: testPage,
}) => {
  const assert = {
    equal: (actual, expected, message) =>
      expect(actual, message).toBe(expected),
    notEqual: (actual, expected, message) =>
      expect(actual, message).not.toBe(expected),
    deepEqual: (actual, expected, message) =>
      expect(actual, message).toEqual(expected),
    ok: (actual, message) => expect(actual, message).toBeTruthy(),
    match: (actual, expected, message) =>
      expect(actual, message).toMatch(expected),
    doesNotMatch: (actual, expected, message) =>
      expect(actual, message).not.toMatch(expected),
    fail: (message) => {
      throw new Error(message);
    },
  };

  const repo = dirname(
    dirname(dirname(dirname(fileURLToPath(import.meta.url)))),
  );
  const require = createRequire(join(repo, "web/package.json"));
  const { createServer: createViteServer } = await import(
    require.resolve("vite")
  );
  const temporary = await mkdtemp(join(tmpdir(), "studio-chat-ux-"));
  const entry = join(temporary, "index.html");
  await writeFile(
    entry,
    `<html><div id="conversation-header-tools"></div><div id="root"></div><script type="module" src="${join(repo, "tests/fixtures/ux-chat.tsx")}"></script></html>`,
  );
  let server;
  let vite;
  const syncStreams = new Set();
  let syncRevision = 0;
  let transcriptSeq = 1;
  const publishTranscriptChange = () => {
    const resource = { kind: "transcript", agentId: "lead" };
    const revision = ++syncRevision;
    for (const stream of syncStreams)
      stream.write(
        `event: resources\ndata: ${JSON.stringify({
          epoch: "ux-chat-epoch",
          protocol: 3,
          reason: "change",
          resources: [resource],
          resourceVersions: [],
          revision,
          workspaceId: "a".repeat(32),
        })}\n\n`,
      );
  };
  try {
    vite = await createViteServer({
      configFile: false,
      root: temporary,
      logLevel: "error",
      server: { middlewareMode: true, hmr: false },
      optimizeDeps: { include: ["react/jsx-dev-runtime"] },
      resolve: {
        alias: [
          { find: /^react$/, replacement: require.resolve("react") },
          {
            find: /^react\/jsx-runtime$/,
            replacement: require.resolve("react/jsx-runtime"),
          },
          {
            find: /^react\/jsx-dev-runtime$/,
            replacement: require.resolve("react/jsx-dev-runtime"),
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
    });
    server = createServer(async (request, response) => {
      try {
        const path = new URL(request.url, "http://fixture").pathname;
        const requestUrl = new URL(request.url, "http://fixture");
        if (path === "/api/sync/stream") {
          const resources = JSON.parse(
            requestUrl.searchParams.get("resources") || "[]",
          );
          response.writeHead(200, {
            "Cache-Control": "no-cache",
            "Content-Type": "text/event-stream",
            [API_SCHEMA_HASH_HEADER]: readApiSchemaHash(),
          });
          response.write(apiSchemaHandshakeSse());
          syncStreams.add(response);
          response.on("close", () => syncStreams.delete(response));
          response.write(
            `event: resources\ndata: ${JSON.stringify({
              epoch: "ux-chat-epoch",
              protocol: 3,
              reason: "initial",
              resources,
              resourceVersions: [],
              revision: ++syncRevision,
              workspaceId: "a".repeat(32),
            })}\n\n`,
          );
          return;
        }
        if (path === "/api/fixture-restore") {
          historyVersion = "restored-version";
          latestItems = records.slice(12);
          transcriptSeq++;
          publishTranscriptChange();
          response.statusCode = 204;
          response.end();
          return;
        }
        if (path === "/api/fixture-live-update") {
          latestItems = latestItems.map((item) =>
            item.id === "item-13"
              ? { ...item, text: "Latest message after live update" }
              : item,
          );
          transcriptSeq++;
          publishTranscriptChange();
          response.statusCode = 204;
          response.end();
          return;
        }
        if (path === "/api/sync/identity") {
          response.setHeader(API_SCHEMA_HASH_HEADER, readApiSchemaHash());
          response.setHeader("Content-Type", "application/json");
          response.end(
            JSON.stringify({ workspaceId: "a".repeat(32), syncProtocol: 2 }),
          );
          return;
        }
        if (path === "/api/sync/protocol") {
          response.setHeader(API_SCHEMA_HASH_HEADER, readApiSchemaHash());
          response.setHeader("Content-Type", "application/json");
          response.end(JSON.stringify(syncProtocolFixture()));
          return;
        }
        if (path === "/api/sync/pull") {
          const scope = requestUrl.searchParams.get("scope");
          const after = Number(requestUrl.searchParams.get("after") || 0);
          const document = {
            id: scope,
            seq: transcriptSeq,
            payload: JSON.stringify(
              snapshot(scope?.slice("transcript:".length)),
            ),
            _deleted: false,
          };
          response.setHeader(API_SCHEMA_HASH_HEADER, readApiSchemaHash());
          response.setHeader("Content-Type", "application/json");
          response.end(
            JSON.stringify({
              workspaceId: "a".repeat(32),
              generation: 1,
              documents:
                scope?.startsWith("transcript:") && after < document.seq
                  ? [document]
                  : [],
              checkpoint: { seq: after < document.seq ? document.seq : after },
              maxSeq: transcriptSeq,
              initialHigh: transcriptSeq,
            }),
          );
          return;
        }
        vite.middlewares(request, response, () => {
          response.statusCode = 404;
          response.end("Not found");
        });
      } catch {
        response.statusCode = 404;
        response.end();
      }
    });
    await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
    const page = testPage;
    const errors = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.setViewportSize({ width: 1100, height: 800 });
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
      pendingAround,
      resolveAroundStarted;
    let aroundStarted = Promise.resolve();
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
      if (path.startsWith("/api/sync/")) return route.continue();
      if (path === "/api/fixture-restore") return route.continue();
      if (path === "/api/fixture-live-update") return route.continue();
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
          if (holdAround) {
            await new Promise((resolve) => {
              pendingAround = resolve;
              resolveAroundStarted?.();
            });
          }
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
    const sendSettled = () =>
      page.evaluate(
        () =>
          new Promise((resolve) => {
            const status = document.querySelector("#send-state");
            let active = !!status.textContent;
            const observer = new MutationObserver(() => {
              if (status.textContent) active = true;
              else if (active) {
                observer.disconnect();
                resolve();
              }
            });
            observer.observe(status, {
              childList: true,
              characterData: true,
              subtree: true,
            });
          }),
      );
    await composer.fill("Draft must remain unsent");
    const firstSettled = sendSettled();
    const firstSend = page.waitForResponse(
      (response) => new URL(response.url()).pathname === "/api/fixture-send",
    );
    await composer.press("Tab");
    await firstSend;
    await firstSettled;
    await page.waitForFunction(
      () => document.querySelector("#message").value === "",
    );
    assert.equal(
      await composer.evaluate((element) => element === document.activeElement),
      true,
    );
    assert.equal(writes.length, 1);
    assert.equal(writes[0].path, "/api/fixture-send");
    assert.equal(writes[0].body.text, "Draft must remain unsent");
    assert.equal(writes[0].body.delivery, "after_turn");
    await composer.fill("Send from button");
    const buttonSend = page.waitForResponse(
      (response) => new URL(response.url()).pathname === "/api/fixture-send",
    );
    const buttonSettled = sendSettled();
    await page.locator("#send").click();
    await buttonSend;
    await buttonSettled;
    await page.waitForFunction(
      () => document.querySelector("#message").value === "",
    );
    assert.equal(writes.length, 2);
    assert.equal(writes[1].body.delivery, "after_tool");
    assert.equal(writes[1].body.text, "Send from button");
    const longDraft = "L".repeat(12050);
    await composer.fill(longDraft);
    assert.equal(await composer.inputValue(), longDraft);
    await page.locator("#draft-length-error").waitFor();
    assert.equal(await page.locator("#send").isDisabled(), true);
    await composer.press("Enter");
    assert.equal(
      writes.filter((write) => write.path === "/api/fixture-send").length,
      2,
    );
    const attach = page.getByRole("button", {
      name: "Attach text as a file",
      exact: true,
    });
    assert.equal(
      await attach.isEnabled(),
      true,
      "Long draft can become a file",
    );
    await attach.click();
    await page.waitForFunction(
      () => document.querySelector("#message").value === "",
    );
    await page
      .getByRole("button", { name: "Remove pending message.txt", exact: true })
      .waitFor();
    const savedUpload = await page.evaluate(
      () =>
        new Promise((resolve, reject) => {
          const request = indexedDB.open("codex-studio-uploads", 2);
          request.onerror = () => reject(request.error);
          request.onsuccess = () => {
            const database = request.result;
            const transaction = database.transaction(
              ["uploads", "contents"],
              "readonly",
            );
            const rows = transaction.objectStore("uploads").getAll();
            rows.onerror = () => reject(rows.error);
            rows.onsuccess = () => {
              const row = rows.result.find(
                (item) => item.name === "message.txt",
              );
              if (!row) return resolve(null);
              const content = transaction.objectStore("contents").get(row.id);
              content.onerror = () => reject(content.error);
              content.onsuccess = () => {
                database.close();
                resolve({
                  name: row.name,
                  bytes: Array.from(new Uint8Array(content.result.bytes)),
                });
              };
            };
          };
        }),
    );
    assert.ok(savedUpload);
    assert.equal(savedUpload.name, "message.txt");
    assert.equal(Buffer.from(savedUpload.bytes).toString(), longDraft);
    await page
      .getByRole("button", { name: "Remove pending message.txt", exact: true })
      .click();

    await page.locator("#earlier-messages").click();
    await page.locator('[data-message="item-6"]').waitFor();
    assert.equal(await page.locator('[data-message="item-13"]').count(), 1);
    await page.evaluate(() =>
      fetch("/api/fixture-live-update", { method: "POST" }),
    );
    await page.waitForFunction(() =>
      document
        .querySelector('[data-message="item-13"]')
        ?.textContent.includes("Latest message after live update"),
    );
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
    aroundStarted = new Promise((resolve) => {
      resolveAroundStarted = resolve;
    });
    await page.evaluate(() => window.chatFixture.jump("item-3"));
    await aroundStarted;
    resolveAroundStarted = undefined;
    assert.ok(pendingAround, "the historical page read is pending");
    await composer.fill("Send while history loads");
    await page.locator("#send").click();
    await page.waitForFunction(
      () => document.querySelector("#message").value === "",
    );
    const releasedAroundResponse = page.waitForResponse((response) => {
      const url = new URL(response.url());
      return (
        url.pathname === "/api/transcript/page" &&
        url.searchParams.get("around") === "item-3"
      );
    });
    holdAround = false;
    releaseAround();
    await releasedAroundResponse;
    await page.evaluate(
      () =>
        new Promise((resolve) =>
          requestAnimationFrame(() => requestAnimationFrame(resolve)),
        ),
    );
    await page.waitForFunction(
      () =>
        document.querySelector('[data-message="item-13"]') &&
        !document.querySelector('[data-message="item-3"]'),
    );
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
    assert.deepEqual(
      writes
        .filter((write) => write.path === "/api/fixture-send")
        .map((write) => write.body.text),
      [
        "Draft must remain unsent",
        "Send from button",
        "Send while history loads",
      ],
      "branch creation never sends the reviewed draft",
    );
    await page.evaluate(() => window.chatFixture.select("lead"));
    await page.locator('[data-message="item-13"]').waitFor();
    assert.equal(await composer.inputValue(), "Source chat draft");
    await page
      .locator('[data-message="item-13"]')
      .getByRole("button", {
        name: "Another answer in a new chat",
        exact: true,
      })
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
    assert.deepEqual(
      writes
        .filter((write) => write.path === "/api/fixture-send")
        .map((write) => write.body.text),
      [
        "Draft must remain unsent",
        "Send from button",
        "Send while history loads",
      ],
    );
    assert.equal(
      writes.filter((write) => write.path === "/api/branch").at(-1).body.before,
      undefined,
    );

    await page.evaluate(() => window.chatFixture.select("lead"));
    await page.locator('[data-message="item-10"]').waitFor();
    await page.locator("#earlier-messages").click();
    await page.locator('[data-message="item-6"]').waitFor();
    await page.evaluate(() =>
      fetch("/api/fixture-restore", { method: "POST" }),
    );
    await page.waitForFunction(
      () => !document.querySelector('[data-message="item-6"]'),
    );
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
      `PASS: chat send, Tab navigation, long draft, complete history, exact search, branch draft and restore boundaries. ${temporary}`,
    );
  } finally {
    for (const stream of syncStreams) stream.destroy();
    if (server) await new Promise((resolve) => server.close(resolve));
    await vite?.close();
    await rm(temporary, { recursive: true, force: true });
  }
});
