import type { components } from "../../src/generated/api.ts";
import { requestBody } from "../messages/helpers.ts";
import { readLocalDraft } from "../drafts/draft-fixture-helpers.ts";
import type { PostBody } from "../../src/apiTypes.ts";
import { fixtureOrigin } from "../drafts/draft-fixture-helpers.ts";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { test } from "../playwright.ts";

test("Draft recovery", async ({ context: testContext }) => {
  test.setTimeout(180_000);
  // Real IndexedDB. Fail local writes, reload, and recover without losing text.
  const { createFixtureViteServer: createServer } =
    await import("../playwright.ts");
  const server = await createServer({
    configFile: false,
    root: fileURLToPath(new URL("../../", import.meta.url)),
    server: { host: "127.0.0.1", port: 0 },
  });
  await server.listen();
  try {
    const page = await testContext.newPage(),
      errors: string[] = [];
    page.on("pageerror", (error) => errors.push(error.message));
    let workspaceId = "b".repeat(32);
    const pushes: PostBody<"/api/sync/drafts">["rows"] = [];
    await page.route("**/check", (route) =>
      route.fulfill({
        contentType: "text/html",
        body: "<!doctype html><title>Draft recovery</title>",
      }),
    );
    await page.route("**/api/sync/identity", (route) =>
      route.fulfill({
        json: { workspaceId } satisfies Pick<
          components["schemas"]["SyncIdentityResponse"],
          "workspaceId"
        >,
      }),
    );
    await page.route("**/api/sync/drafts", (route) => {
      pushes.push(...requestBody<PostBody<"/api/sync/drafts">>(route).rows);
      return route.fulfill({ json: [] });
    });
    await page.route("**/api/sync/pull?*", (route) =>
      route.fulfill({
        json: {
          workspaceId,
          documents: [],
          checkpoint: { seq: 0 },
        } satisfies Pick<
          components["schemas"]["SyncPullResponse"],
          "workspaceId" | "documents" | "checkpoint"
        >,
      }),
    );
    const origin = fixtureOrigin(server);
    const mount = async (fail: boolean) => {
      await page.goto(origin + "/check");
      await page.evaluate(async (fail: boolean) => {
        const { mount } =
          await import("/src/../tests/drafts/draft-recovery-browser.fixture.tsx");
        await mount(fail);
      }, fail);
      await page.waitForFunction(() => !!window.recoveryDraft_draft);
    };
    const edit = (text: string) =>
      page.evaluate(
        (text: string) =>
          window.recoveryDraft_draft.setDrafts((old) => ({
            ...old,
            lead: text,
          })),
        text,
      );
    const stored = (text: string) =>
      page.waitForFunction(
        async (text: string) =>
          (await window.recoveryDraft_db.drafts.find().exec()).some(
            (doc) => window.recoveryDraft_readDraft(doc.payload).text === text,
          ),
        text,
      );
    await mount(false);
    await edit("Saved version");
    await stored("Saved version");
    await page.evaluate(() => {
      window.recoveryDraft_failWrites = true;
      window.recoveryDraft_blockDraftJournal = true;
      window.recoveryDraft_originalSetItem = Storage.prototype.setItem;
      Storage.prototype.setItem = function (key, value) {
        if (window.recoveryDraft_blockDraftJournal && key.includes(":pending:"))
          throw new Error("Injected journal write failure");
        return window.recoveryDraft_originalSetItem.call(this, key, value);
      };
    });
    await edit("Journal failure retries from the current draft record");
    await page.waitForFunction(() => !!window.recoveryDraft_draft.error);
    const journalFailure = await page.evaluate(() => ({
      error: window.recoveryDraft_draft.error,
      record: Object.entries(localStorage).find(([key]) =>
        key.startsWith("codex-chat-draft:"),
      )?.[1],
      pending: Object.keys(localStorage).some((key) =>
        key.includes(":pending:"),
      ),
    }));
    assert.ok(journalFailure.error);
    assert.ok(journalFailure.record);
    assert.equal(
      readLocalDraft(journalFailure.record).text,
      "Journal failure retries from the current draft record",
    );
    assert.equal(journalFailure.pending, false);
    await page.evaluate(() => {
      window.recoveryDraft_blockDraftJournal = false;
      Storage.prototype.setItem = window.recoveryDraft_originalSetItem;
      window.recoveryDraft_failWrites = false;
      window.dispatchEvent(new Event("pageshow"));
    });
    await stored("Journal failure retries from the current draft record");
    await page.waitForFunction(() => !window.recoveryDraft_draft.error);
    await page.evaluate(() => {
      window.recoveryDraft_failWrites = true;
    });
    await edit("New draft during a write failure");
    await page.waitForFunction(() => !!window.recoveryDraft_draft.error);
    await mount(true);
    await page.waitForTimeout(500);
    assert.equal(
      await page.evaluate(() => window.recoveryDraft_draft.drafts.lead),
      "New draft during a write failure",
      "Reload must not restore the older IndexedDB value over the unsaved edit",
    );
    await page.evaluate(() => {
      window.recoveryDraft_failWrites = false;
      window.dispatchEvent(new Event("pageshow"));
    });
    await stored("New draft during a write failure");
    await page.waitForFunction(() => !window.recoveryDraft_draft.error);
    await page.evaluate(() => {
      window.recoveryDraft_failWrites = true;
    });
    await edit("");
    await page.waitForFunction(() => !!window.recoveryDraft_draft.error);
    await mount(true);
    await page.waitForTimeout(500);
    assert.equal(
      await page.evaluate(() => window.recoveryDraft_draft.drafts.lead),
      "",
      "An unsaved clear must remain empty after reload",
    );
    await page.evaluate(() => {
      window.recoveryDraft_failWrites = false;
      window.dispatchEvent(new Event("pageshow"));
    });
    await stored("");
    await page.waitForFunction(() => !window.recoveryDraft_draft.error);
    await page.evaluate(() => {
      for (let i = 0; i < 20; i++)
        window.recoveryDraft_draft.setDrafts((old) => ({
          ...old,
          lead: `Rapid edit ${i}`,
        }));
    });
    await stored("Rapid edit 19");
    await page.waitForFunction(() => !window.recoveryDraft_draft.error);
    assert.equal(
      await page.evaluate(() => window.recoveryDraft_draft.conflicts.length),
      0,
      "Typing must not create conflicts with earlier keystrokes",
    );
    await page.evaluate(() => {
      window.recoveryDraft_failWrites = true;
    });
    await edit("Workspace A draft");
    await page.waitForFunction(() => !!window.recoveryDraft_draft.error);
    workspaceId = "e".repeat(32);
    await mount(true);
    await page.waitForTimeout(500);
    assert.equal(
      await page.evaluate(() => window.recoveryDraft_draft.drafts.lead),
      undefined,
      "Pending drafts must not move to a different workspace",
    );
    assert.ok(
      await page.evaluate(() =>
        Object.keys(localStorage).some((key) =>
          key.startsWith(`codex-drafts:${"b".repeat(32)}:pending:`),
        ),
      ),
    );
    workspaceId = "b".repeat(32);
    await mount(false);
    await stored("Workspace A draft");
    await page.waitForFunction(
      () => window.recoveryDraft_draft.drafts.lead === "Workspace A draft",
    );
    // A recovered old write preserves, rather than overwrites, a newer tab's text.
    await page.evaluate(() => {
      window.recoveryDraft_failWrites = true;
    });
    await edit("Recovered concurrent draft");
    await page.waitForFunction(() => !!window.recoveryDraft_draft.error);
    await page.evaluate(async () => {
      window.recoveryDraft_failWrites = false;
      const doc = (await window.recoveryDraft_db.drafts.find().exec())[0];
      const pendingKey = Object.keys(localStorage).find((key) =>
        key.includes(":pending:"),
      );
      if (!pendingKey || !doc) throw new Error("Missing pending draft");
      const pending = window.recoveryDraft_readDraft(
        localStorage.getItem(pendingKey) ?? "null",
      );
      const payload = {
        ...window.recoveryDraft_readDraft(doc.payload),
        text: "Newer tab draft",
        updated: pending.updated + 10,
        seen: {},
        alternatives: [],
      };
      await window.recoveryDraft_db.drafts.incrementalUpsert({
        id: doc.id,
        seq: 0,
        payload: JSON.stringify(payload),
      });
      window.recoveryDraft_failWrites = true;
    });
    await mount(false);
    await page.waitForFunction(
      () =>
        window.recoveryDraft_draft.drafts.lead === "Newer tab draft" &&
        window.recoveryDraft_draft.conflicts.some(
          (version) => version.text === "Recovered concurrent draft",
        ),
    );
    assert.deepEqual(errors, []);
    console.log(
      "PASS: local write failure, reload, exact draft recovery, clear recovery, rapid edits, workspace isolation, concurrent text, automatic retry",
    );
  } finally {
    await server.close();
  }
});
