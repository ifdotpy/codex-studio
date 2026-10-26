import type { components } from "../../src/generated/api.ts";
import { requestBody } from "../messages/helpers.ts";
import type { PostBody, GetResult } from "../../src/apiTypes.ts";
import { fixtureOrigin } from "../drafts/draft-fixture-helpers.ts";
// Real components, isolated Vite server, mocked requests. No model or user state.
import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

import { test, expect } from "../playwright.ts";

test("dialog-drafts-ui", async ({ page: fixturePage }) => {
  test.setTimeout(120_000);
  const repo = fileURLToPath(new URL("../../../../../../", import.meta.url));
  const { createFixtureViteServer: createServer } =
    await import("../playwright.ts");
  const cache = await mkdtemp(join(tmpdir(), "dialog-drafts-vite-"));

  const server = await createServer({
    configFile: false,
    cacheDir: cache,
    root: join(repo, "workspaces/client/apps/web"),
    server: { host: "127.0.0.1", port: 0 },
    plugins: [
      {
        name: "dialog-fixture",
        configureServer(server) {
          server.middlewares.use((req, res, next) => {
            if (req.url !== "/") return next();
            res.setHeader("Content-Type", "text/html");
            res.end(
              '<div id="root"></div><script type="module" src="/tests/drafts/dialog-drafts-ui.fixture.tsx"></script>',
            );
          });
        },
      },
    ],
  });
  try {
    await server.listen();
    const page = fixturePage;
    await fixturePage.setViewportSize({ width: 1440, height: 960 });
    const pageErrors: string[] = [];
    page.on("pageerror", (e) => pageErrors.push(e.message));
    let directoryReads = 0,
      detailReads = 0,
      releaseSend = () => {};
    const sent: PostBody<"/api/complaints">[] = [];
    const waitForSent = async (count: number) => {
      for (let attempt = 0; attempt < 100 && sent.length < count; attempt++)
        await new Promise((resolve) => setTimeout(resolve, 10));
      expect(sent.length).toBe(count);
    };
    let message: GetResult<"/api/complaint"> = {
      id: "complaint",
      recipient: "user",
      author: "lead",
      leadId: "lead",
      version: 1,
      text: "Please help",
      responses: [],
      status: "open",
    };
    await page.route("**/api/**", async (route) => {
      const path = new URL(route.request().url()).pathname;
      if (path === "/api/directories") {
        directoryReads++;
        return route.fulfill({
          json: {
            path: "/project",
            parent: null,
            directories: [],
          } satisfies components["schemas"]["DirectoriesResponse"],
        });
      }
      if (path === "/api/complaint") {
        detailReads++;
        if (detailReads === 1)
          return route.fulfill({
            status: 503,
            json: { error: "Temporary failure" },
          });
        return route.fulfill({ json: message });
      }
      if (path === "/api/complaints") {
        const body = requestBody<PostBody<"/api/complaints">>(route);
        if (!body.status) throw new Error("Expected response status");
        sent.push(body);
        if (sent.length === 1)
          return route.fulfill({
            status: 503,
            json: { error: "Lost reply response" },
          });
        await new Promise<void>((resolve) => {
          releaseSend = resolve;
        });
        message = {
          ...message,
          version: message.version + 1,
          status: body.status,
          responses: [
            ...message.responses,
            {
              id: body.id,
              author: "user",
              text: body.text,
              status: body.status,
              at: 1,
            },
          ],
        };
        return route.fulfill({ json: message });
      }
      return route.fulfill({ json: {} });
    });
    await page.goto(fixtureOrigin(server));
    await page
      .locator("[data-missing-date]")
      .getByText("Date unavailable", { exact: true })
      .waitFor();
    assert.equal(
      await page.locator("[data-missing-date] time").getAttribute("datetime"),
      null,
    );
    const useFolder = page.getByRole("button", {
      name: "Add project",
      exact: true,
    });
    await useFolder.waitFor();
    await page.waitForFunction(
      () =>
        !document.querySelector<HTMLButtonElement>(".directory-footer button")
          ?.disabled,
    );
    await page.getByRole("button", { name: "Open path", exact: true }).click();
    await page.waitForFunction(
      () =>
        !document.querySelector<HTMLButtonElement>(".directory-footer button")
          ?.disabled,
    );
    assert.equal(directoryReads, 2);
    await page.getByRole("button", { name: "Answer", exact: true }).click();
    await page
      .getByLabel("Public answer", { exact: true })
      .fill("Keep the public answer");
    await page
      .getByLabel("Private answer", { exact: true })
      .fill("DO-NOT-STORE-SECRET");
    assert.equal(
      await page.evaluate(() =>
        JSON.stringify(localStorage).includes("DO-NOT-STORE-SECRET"),
      ),
      false,
    );
    await page.reload();
    await page.getByRole("button", { name: "Answer", exact: true }).click();
    assert.equal(
      await page.getByLabel("Public answer", { exact: true }).inputValue(),
      "Keep the public answer",
    );
    assert.equal(
      await page.getByLabel("Private answer", { exact: true }).inputValue(),
      "",
    );
    await page
      .getByRole("button", { name: "Switch workspace", exact: true })
      .click();
    await page.getByRole("button", { name: "Answer", exact: true }).click();
    assert.equal(
      await page.getByLabel("Public answer", { exact: true }).inputValue(),
      "",
    );
    await page
      .getByRole("button", { name: "Switch workspace", exact: true })
      .click();
    await page.getByRole("button", { name: "Answer", exact: true }).click();
    assert.equal(
      await page.getByLabel("Public answer", { exact: true }).inputValue(),
      "Keep the public answer",
    );
    await page.evaluate(() => {
      const original = Storage.prototype.setItem;
      window.dialogDraft_restoreStorage = () =>
        (Storage.prototype.setItem = original);
      Storage.prototype.setItem = function (key, value) {
        if (key.startsWith("studio-answer-draft:"))
          throw new DOMException("Full", "QuotaExceededError");
        return original.call(this, key, value);
      };
    });
    await page
      .getByLabel("Public answer", { exact: true })
      .fill("Unsaved answer remains visible");
    assert.equal(
      await page.getByLabel("Public answer", { exact: true }).inputValue(),
      "Unsaved answer remains visible",
    );
    assert.match(
      await page.evaluate(() => window.dialogDraft_notices.at(-1) ?? ""),
      /could not be saved/,
    );
    await page.evaluate(() => window.dialogDraft_restoreStorage());
    const toggle = () =>
      page
        .getByRole("button", { name: "Toggle messages", exact: true })
        .click();
    await toggle();
    const detail = page.locator('[data-complaint="complaint"]');
    await detail.getByRole("alert").waitFor();
    assert.equal(
      await detail.getByText("Loading message…", { exact: true }).count(),
      0,
    );
    await detail.getByRole("button", { name: "Retry", exact: true }).click();
    await detail.getByText("Please help", { exact: true }).waitFor();
    assert.equal(await detail.getByRole("alert").count(), 0);
    const openReply = () =>
      detail.getByRole("button", { name: "Reply", exact: true }).click();
    const reply = detail.getByLabel("Reply", { exact: true });
    await openReply();
    await reply.fill("Original text");
    await detail
      .getByRole("button", { name: "Close reply", exact: true })
      .click();
    await openReply();
    assert.equal(await reply.inputValue(), "Original text");
    await toggle();
    await toggle();
    await openReply();
    assert.equal(
      await reply.inputValue(),
      "Original text",
      "Reply survives remount",
    );
    await page
      .getByRole("button", { name: "Switch workspace", exact: true })
      .click();
    await openReply();
    assert.equal(await reply.inputValue(), "");
    await reply.fill("Separate reply");
    await page
      .getByRole("button", { name: "Switch workspace", exact: true })
      .click();
    await openReply();
    assert.equal(
      await reply.inputValue(),
      "Original text",
      "Reply drafts belong to a workspace",
    );
    await page.reload();
    await toggle();
    await openReply();
    assert.equal(
      await reply.inputValue(),
      "Original text",
      "Reply survives reload",
    );
    await detail
      .getByRole("button", { name: "Send reply", exact: true })
      .click();
    await detail
      .getByRole("button", { name: "Retry response", exact: true })
      .waitFor();
    assert.equal(sent.length, 1);
    await page
      .getByRole("button", { name: "Switch workspace", exact: true })
      .click();
    await openReply();
    assert.equal(
      await reply.inputValue(),
      "Separate reply",
      "Another workspace cannot inherit the pending payload",
    );
    assert.equal(await reply.isDisabled(), false);
    assert.equal(
      await detail
        .getByRole("button", { name: "Retry response", exact: true })
        .count(),
      0,
    );
    await page
      .getByRole("button", { name: "Switch workspace", exact: true })
      .click();
    await detail
      .getByRole("button", { name: "Retry response", exact: true })
      .waitFor();
    assert.equal(await reply.inputValue(), "Original text");
    await page.reload();
    await toggle();
    await detail
      .getByRole("button", { name: "Retry response", exact: true })
      .waitFor();
    assert.equal(await reply.inputValue(), "Original text");
    assert.equal(await reply.isDisabled(), true);
    await detail
      .getByRole("button", { name: "Retry response", exact: true })
      .click();
    await waitForSent(2);
    assert.deepEqual(
      sent[1],
      sent[0],
      "Retry after reload retains the exact message identity and payload",
    );
    releaseSend();
    await page.waitForFunction(
      () =>
        !document.querySelector<HTMLTextAreaElement>(
          ".complaint-reply textarea",
        )?.disabled,
    );
    assert.equal(await reply.inputValue(), "");
    await detail
      .locator(".complaint-response")
      .getByText("Original text")
      .waitFor();
    await reply.fill("New reply");
    await detail
      .getByRole("button", { name: "Send reply", exact: true })
      .click();
    await waitForSent(3);
    assert.equal(await reply.isDisabled(), true);
    releaseSend();
    await detail
      .locator(".complaint-response")
      .getByText("New reply")
      .waitFor();
    assert.equal(sent[2]?.text, "New reply");
    assert.equal(sent[0]?.action, "respond");
    assert.equal(sent[0]?.complaint_id, "complaint");
    assert.equal(sent[0]?.version, 1);
    assert.equal(sent[2]?.version, 2);
    assert.notEqual(sent[0]?.id, sent[2]?.id);
    assert.deepEqual(pageErrors, []);
    console.log(
      JSON.stringify({
        ok: true,
        checks: [
          "same-folder retry",
          "message reply collapse, remount and workspace isolation",
          "message detail error and retry",
          "reply draft retained after reopen and reload",
          "non-secret answers survive reload, secrets do not persist",
          "answer workspace isolation and storage failure notice",
          "message reply retry retains its exact identity after reload",
          "reply fixed during send, subsequent response uses new identity and version",
        ],
      }),
    );
  } finally {
    await server.close();
    await rm(cache, { recursive: true, force: true });
  }
});
