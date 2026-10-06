// Real renderer tabs exercise every direct sync entity writer through the
// common SQLite post-commit StateResource invalidation.
import assert from "node:assert/strict";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnFixture, test } from "../playwright.mjs";

test("committed entities propagate between renderer tabs @sync", async ({
  browser,
}) => {
  test.setTimeout(120_000);
  const repo = dirname(
    dirname(dirname(dirname(fileURLToPath(import.meta.url)))),
  );
  const state = await mkdtemp(join(tmpdir(), "entity-commit-propagation-"));
  const fixture = spawnFixture(
    "python3",
    ["-B", join(repo, "tests/simple-ui-fixture.py"), state],
    {
      stdio: ["pipe", "pipe", "pipe"],
      env: { ...process.env, RESOURCE_FRESHNESS_UI_FIXTURE: "1" },
    },
  );
  let log = "";
  let output = "";
  const fixtureReplies = new Map();
  const fixtureWaiters = new Map();
  fixture.stdout.on("data", (chunk) => {
    output += String(chunk);
    const lines = output.split("\n");
    output = lines.pop() || "";
    for (const line of lines) {
      try {
        const reply = JSON.parse(line);
        const waiter = fixtureWaiters.get(reply.id);
        if (waiter) {
          clearTimeout(waiter.timeout);
          fixtureWaiters.delete(reply.id);
          waiter.resolve(reply);
        } else if (reply.id) fixtureReplies.set(reply.id, reply);
      } catch {}
    }
  });
  fixture.stderr.on("data", (chunk) => (log += String(chunk)));
  const port = await new Promise((resolve, reject) => {
    fixture.stdout.once("data", (chunk) =>
      resolve(Number(String(chunk).trim())),
    );
    fixture.once("exit", () => reject(new Error(log)));
  });
  const context = await browser.newContext();
  const pages = [];
  const pulls = [[], []];
  const waitFor = async (predicate, message, timeoutMs = 8000) => {
    const end = Date.now() + timeoutMs;
    while (Date.now() < end) {
      if (predicate()) return;
      await new Promise((resolve) => setTimeout(resolve, 40));
    }
    assert.fail(message);
  };
  const command = async (id, params) => {
    const reply = fixtureReplies.has(id)
      ? Promise.resolve(fixtureReplies.get(id))
      : new Promise((resolve, reject) => {
          const timeout = setTimeout(() => reject(new Error(log)), 8000);
          fixtureWaiters.set(id, { resolve, reject, timeout });
        });
    fixture.stdin.write(
      `${JSON.stringify({ id, method: "fixture/entity-change", params })}\n`,
    );
    return reply;
  };
  try {
    for (let index = 0; index < 2; index++) {
      const page = await context.newPage();
      page.setDefaultTimeout(10000);
      await page.addInitScript(() => {
        const OriginalEventSource = window.EventSource;
        window.__entityFrames = [];
        window.EventSource = class extends OriginalEventSource {
          constructor(...args) {
            super(...args);
            this.addEventListener("resources", (event) =>
              window.__entityFrames.push({
                at: Date.now(),
                event: JSON.parse(event.data),
              }),
            );
          }
        };
      });
      page.on("response", async (response) => {
        const url = new URL(response.url());
        if (
          url.pathname !== "/api/sync/pull" ||
          url.searchParams.get("scope") !== "state:entities:v1"
        )
          return;
        try {
          const body = await response.json();
          pulls[index].push({
            at: Date.now(),
            documents: body.documents || [],
          });
        } catch {}
      });
      await page.goto(`http://127.0.0.1:${port}`);
      await page
        .locator("[data-chat]")
        .filter({ hasText: "Release lead" })
        .click();
      pages.push(page);
    }
    await waitFor(
      () => pulls.every((rows) => rows.length > 0),
      "both tabs did not finish their initial entity pull",
    );
    const leadId = await pages[0].evaluate(async () => {
      const response = await fetch("/api/state");
      const state = await response.json();
      return state.threads.find((agent) => agent.name === "Release lead").id;
    });
    const workspaceId = await pages[0].evaluate(() =>
      fetch("/api/sync/identity")
        .then((response) => response.json())
        .then((value) => value.workspaceId),
    );
    const results = [];
    const operations = [
      {
        operation: "chat",
        id: crypto.randomUUID(),
        name: "Cross-tab new chat",
        collection: "agent",
        entityId: null,
        deleted: false,
      },
      { operation: "project", collection: "project", deleted: true },
      {
        operation: "rule",
        id: crypto.randomUUID(),
        agent: leadId,
        collection: "rule",
        entityId: null,
        deleted: true,
      },
      {
        operation: "budget",
        agent: leadId,
        collection: "agent",
        entityId: leadId,
        deleted: false,
      },
    ];
    for (const [index, params] of operations.entries()) {
      const previousPulls = pulls[1].length;
      const actingTabPreviousPulls = pulls[0].length;
      const previousFrames = await Promise.all(
        pages.map((page) => page.evaluate(() => window.__entityFrames.length)),
      );
      const commandId = `entity-change-${index}`;
      const startedAt = Date.now();
      let reply;
      if (params.operation === "chat") {
        reply = await pages[0].evaluate(
          async ({ chat, workspaceId, cwd }) => {
            const { token } = await fetch("/api/session").then((response) =>
              response.json(),
            );
            window.dispatchEvent(new Event("codex-api-mutation-start"));
            const dispatchEntities = (body) => {
              if (body._syncEntities?.length) {
                window.dispatchEvent(
                  new CustomEvent("codex-sync-entities", {
                    detail: { workspaceId, documents: body._syncEntities },
                  }),
                );
              }
            };
            const created = await fetch("/api/leads", {
              method: "POST",
              headers: {
                "content-type": "application/json",
                "X-Canvas-Token": token,
              },
              body: JSON.stringify({ id: chat.id, cwd }),
            });
            const createdBody = await created.json();
            dispatchEntities(createdBody);
            const renamed = await fetch("/api/rename", {
              method: "POST",
              headers: {
                "content-type": "application/json",
                "X-Canvas-Token": token,
              },
              body: JSON.stringify({ id: chat.id, name: chat.name }),
            });
            const body = await renamed.json();
            dispatchEntities(body);
            window.dispatchEvent(new Event("codex-api-mutation-end"));
            return {
              ok: created.ok && renamed.ok,
              entityId: chat.id,
              seq: Math.max(
                ...(body._syncEntities || createdBody._syncEntities || []).map(
                  (document) => document.seq,
                ),
              ),
              collection: "agent",
              deleted: false,
            };
          },
          { chat: params, workspaceId },
        );
      } else {
        reply = await command(commandId, params);
      }
      assert.equal(reply.ok, true, JSON.stringify(reply));
      if (params.operation === "chat")
        assert.equal(
          pulls[0].length,
          actingTabPreviousPulls,
          "tab A must not pull entity rows already present in its mutation response",
        );
      const entityId = reply.entityId;
      const matchingDocument = (pull) =>
        pull.documents.find(
          (document) =>
            document.id === `entity:${params.collection}:${entityId}` &&
            document.seq >= reply.seq &&
            document._deleted === params.deleted,
        );
      let staleAfterMs;
      try {
        await waitFor(
          () => pulls[1].slice(previousPulls).some(matchingDocument),
          `${params.operation} did not reach tab B through its entity pull within 8s`,
        );
      } catch {
        staleAfterMs = 8000;
      }
      if (staleAfterMs !== undefined) {
        results.push({
          operation: params.operation,
          entityId,
          seq: reply.seq,
          elapsedMs: null,
          staleAfterMs,
        });
        continue;
      }
      const observed = pulls[1]
        .slice(previousPulls)
        .find((pull) => matchingDocument(pull));
      const document = matchingDocument(observed);
      if (params.operation === "chat") {
        await pages[1]
          .locator("[data-chat]")
          .filter({ hasText: params.name })
          .waitFor();
        assert.equal(JSON.parse(document.payload).value.name, params.name);
      }
      if (params.operation === "budget")
        assert.ok(JSON.parse(document.payload).value.tokensUsed >= 13);
      const observedAt = observed.at;
      const changedFrames = await Promise.all(
        pages.map((page, tab) =>
          page.evaluate(
            ({ from, startedAt }) =>
              window.__entityFrames.slice(from).map(({ at, event }) => ({
                elapsedMs: at - startedAt,
                reason: event.reason,
                resources: event.resources,
                resourceVersions: event.resourceVersions,
              })),
            { from: previousFrames[tab], startedAt },
          ),
        ),
      );
      results.push({
        operation: params.operation,
        entityId,
        seq: reply.seq,
        elapsedMs: observedAt - startedAt,
        changedFrames,
        pulls: pulls.map(
          (rows, tab) =>
            rows.length - (tab === 0 ? actingTabPreviousPulls : previousPulls),
        ),
      });
    }
    console.log(
      `ENTITY_PROPAGATION ${JSON.stringify({ results, tabPullCounts: pulls.map((rows) => rows.length) })}`,
    );
    assert.ok(
      results.every(
        (result) =>
          result.staleAfterMs === undefined && result.elapsedMs < 8000,
      ),
    );
  } finally {
    await context.close();
    fixture.kill("SIGTERM");
  }
});
