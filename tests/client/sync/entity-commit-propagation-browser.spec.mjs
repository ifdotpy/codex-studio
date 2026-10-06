// Real renderer tabs exercise every direct sync entity writer through the
// common SQLite post-commit StateResource invalidation.
import assert from "node:assert/strict";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { performance } from "node:perf_hooks";
import { spawnFixture, test } from "../playwright.mjs";

const timestamp = () => performance.timeOrigin + performance.now();

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
  const contexts = [
    await browser.newContext(),
    await browser.newContext(),
    await browser.newContext(),
  ];
  const pages = [];
  const pulls = contexts.map(() => []);
  const statePullRequestCounts = contexts.map(() => 0);
  const pullStarts = contexts.map(() => new WeakMap());
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
    for (let index = 0; index < contexts.length; index++) {
      const page = await contexts[index].newPage();
      page.setDefaultTimeout(10000);
      page.on("request", (request) => {
        const url = new URL(request.url());
        if (
          url.pathname === "/api/sync/pull" &&
          url.searchParams.get("scope") === "state:entities:v1"
        ) {
          statePullRequestCounts[index]++;
          pullStarts[index].set(request, {
            at: timestamp(),
            after: url.searchParams.get("after"),
          });
        }
      });
      await page.addInitScript(() => {
        const OriginalEventSource = window.EventSource;
        window.__entityFrames = [];
        window.__entityStreamUrls = [];
        window.EventSource = class extends OriginalEventSource {
          constructor(...args) {
            super(...args);
            const streamIndex = window.__entityStreamUrls.length;
            window.__entityStreamUrls.push(String(args[0]));
            this.addEventListener("resources", (event) =>
              window.__entityFrames.push({
                at: performance.timeOrigin + performance.now(),
                streamIndex,
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
            at: timestamp(),
            startedAt:
              pullStarts[index].get(response.request())?.at ?? timestamp(),
            after: pullStarts[index].get(response.request())?.after,
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
      "all tabs did not finish their initial entity pull",
    );
    await waitFor(
      () =>
        Promise.all(
          pages.map((page) =>
            page.evaluate(() => {
              const lastStream = window.__entityStreamUrls.length - 1;
              return (
                // The fourth URL is the chat transcript stage after the shell
                // baseline, drafts/state, foreground transcript, and prefetch stages.
                window.__entityStreamUrls.length >= 4 &&
                window.__entityFrames.some(
                  ({ event, streamIndex }) =>
                    streamIndex === lastStream &&
                    event.reason === "initial" &&
                    event.resources.some(
                      (resource) => resource.kind === "state",
                    ),
                )
              );
            }),
          ),
        ).then((ready) => ready.every(Boolean)),
      "all tabs did not complete the shell, drafts/state, foreground transcript, prefetch, and opened-chat stream stages",
    );
    for (const page of pages) {
      const urls = await page.evaluate(() => window.__entityStreamUrls);
      assert.equal(
        new Set(urls).size,
        urls.length,
        "a stream URL was reopened unchanged",
      );
    }
    const leadId = await pages[0].evaluate(async () => {
      const response = await fetch("/api/state");
      const state = await response.json();
      return state.threads.find((agent) => agent.name === "Release lead").id;
    });
    const results = [];
    const operations = [
      {
        operation: "chat",
        collection: "agent",
        entityId: null,
        deleted: false,
      },
      {
        operation: "rename",
        agent: leadId,
        name: "Cross-tab renamed lead",
        collection: "agent",
        entityId: leadId,
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
      const previousPulls = pulls.map((rows) => rows.length);
      const previousFrames = await Promise.all(
        pages.map((page) => page.evaluate(() => window.__entityFrames.length)),
      );
      const commandId = `entity-change-${index}`;
      const startedAt = Date.now();
      let reply;
      if (params.operation === "chat") {
        await pages[0].locator(".project-tree-heading").first().hover();
        const creationResponse = pages[0].waitForResponse(
          (response) =>
            response.request().method() === "POST" &&
            new URL(response.url()).pathname === "/api/leads",
        );
        await pages[0]
          .getByRole("button", { name: /^New chat in / })
          .first()
          .click();
        const created = await creationResponse;
        const responseReceivedAt = timestamp();
        const createdBody = await created.json();
        assert.equal(created.status(), 200);
        assert.ok(createdBody._syncEntities?.length);
        await pages[0]
          .locator("#conversation-title")
          .getByText("New chat", { exact: true })
          .waitFor();
        const responseSequences = createdBody._syncEntities.map(
          (doc) => doc.seq,
        );
        const persister = await pages[0].evaluate((sequences) => {
          const entry = performance
            .getEntriesByName("studio-sync-entity-persister-done", "mark")
            .reverse()
            .find((mark) =>
              sequences.every((sequence) =>
                mark.detail?.sequences?.includes(sequence),
              ),
            );
          if (!entry)
            throw new Error("sync entity persister completion mark missing");
          return {
            at: performance.timeOrigin + entry.startTime,
            ...entry.detail,
          };
        }, responseSequences);
        reply = {
          ok: true,
          entityId: createdBody.id,
          seq: Math.max(...createdBody._syncEntities.map((doc) => doc.seq)),
          deliveredSequences: createdBody._syncEntities.map((doc) => doc.seq),
          // S3's post() resolves only after the renderer persister finishes.
          responseReceivedAt,
          persisterDoneAt: persister.at,
          checkpointBefore: persister.checkpointBefore,
          checkpointAfter: persister.checkpointAfter,
          activeProjection: persister.activeProjection,
          collection: "agent",
          deleted: false,
        };
      } else {
        reply = await command(commandId, params);
      }
      assert.equal(reply.ok, true, JSON.stringify(reply));
      const entityId = reply.entityId;
      const matchingDocument = (pull) =>
        pull.documents.find(
          (document) =>
            document.id === `entity:${params.collection}:${entityId}` &&
            document.seq >= reply.seq &&
            document._deleted === params.deleted,
        );
      const tabsToConfirm = params.operation === "chat" ? [1, 2] : [0, 1, 2];
      let staleAfterMs;
      try {
        await waitFor(
          () =>
            tabsToConfirm.every((tab) =>
              pulls[tab].slice(previousPulls[tab]).some(matchingDocument),
            ),
          `${params.operation} did not reach required tabs through entity pulls within 8s`,
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
          pulls: pulls.map((rows, tab) =>
            rows
              .slice(previousPulls[tab])
              .map((pull) => pull.documents.map((row) => row.seq)),
          ),
          stateFrames: await Promise.all(
            pages.map((page, tab) =>
              page.evaluate(
                (from) => window.__entityFrames.slice(from),
                previousFrames[tab],
              ),
            ),
          ),
        });
        continue;
      }
      const observedTab = params.operation === "chat" ? 1 : 0;
      const observed = pulls[observedTab]
        .slice(previousPulls[observedTab])
        .find((pull) => matchingDocument(pull));
      const document = matchingDocument(observed);
      if (params.operation === "chat") {
        const responseSequences = new Set(reply.deliveredSequences);
        const actorFramesAfterMutation = await pages[0].evaluate(
          (from) =>
            window.__entityFrames.slice(from).map(({ at, event }) => ({
              at,
              reason: event.reason,
              resources: event.resources,
              resourceVersions: event.resourceVersions,
            })),
          previousFrames[0],
        );
        const actorPullTimings = pulls[0]
          .slice(previousPulls[0])
          .map(({ startedAt, after, at, documents }) => ({
            startedAt,
            after,
            completedAt: at,
            sequences: documents.map((row) => row.seq),
          }));
        console.log(
          `ENTITY_MUTATION_ORDER ${JSON.stringify({
            responseReceivedAt: reply.responseReceivedAt,
            persisterDoneAt: reply.persisterDoneAt,
            checkpointBefore: reply.checkpointBefore,
            checkpointAfter: reply.checkpointAfter,
            activeProjection: reply.activeProjection,
            frameArrivals: actorFramesAfterMutation,
            actorPulls: actorPullTimings,
          })}`,
        );
        const actorDuplicatePulls = pulls[0]
          .slice(previousPulls[0])
          .filter(
            (pull) =>
              pull.startedAt > reply.persisterDoneAt &&
              pull.documents.some((row) => responseSequences.has(row.seq)),
          );
        console.log(
          `ENTITY_ACTOR_DUPLICATE_PULLS ${JSON.stringify({
            count: actorDuplicatePulls.length,
            pulls: actorDuplicatePulls.map(
              ({ startedAt, after, documents }) => ({
                startedAt,
                after,
                sequences: documents.map((row) => row.seq),
              }),
            ),
            responseSequences: [...responseSequences],
          })}`,
        );
        assert.ok(
          actorDuplicatePulls.length <= 1,
          `the acting tab may make at most one already-in-flight duplicate pull per mutation: ${JSON.stringify(
            {
              operation: params.operation,
              responseReceivedAt: reply.responseReceivedAt,
              persisterDoneAt: reply.persisterDoneAt,
              checkpointBefore: reply.checkpointBefore,
              checkpointAfter: reply.checkpointAfter,
              responseSequences: [...responseSequences],
              pullStarts: pulls[0]
                .slice(previousPulls[0])
                .map(({ startedAt, after, documents }) => ({
                  startedAt,
                  after,
                  sequences: documents.map((row) => row.seq),
                })),
              frameArrivals: actorFramesAfterMutation,
            },
          )}`,
        );
      }
      if (params.operation === "chat") {
        await pages[1].locator(`[data-chat="${entityId}"]`).waitFor();
        await pages[2].locator(`[data-chat="${entityId}"]`).waitFor();
      }
      if (params.operation === "rename") {
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
        ...(params.operation === "chat"
          ? {
              responseReceivedAt: reply.responseReceivedAt,
              persisterDoneAt: reply.persisterDoneAt,
              frameArrivals: changedFrames[0].map(
                ({ elapsedMs }) => startedAt + elapsedMs,
              ),
              actorPullStarts: pulls[0]
                .slice(previousPulls[0])
                .map(({ startedAt: pullStartedAt }) => pullStartedAt),
            }
          : {}),
        ...(params.operation === "chat"
          ? {
              deliveredSequences: reply.deliveredSequences,
            }
          : {}),
        elapsedMs: observedAt - startedAt,
        changedFrames,
        pulls: pulls.map((rows, tab) => rows.length - previousPulls[tab]),
      });
    }
    const batchPulls = pulls.map((rows) => rows.length);
    const batchFrames = await Promise.all(
      pages.map((page) =>
        page.evaluate(
          () =>
            window.__entityFrames.filter(({ event }) =>
              event.resources.some((resource) => resource.kind === "state"),
            ).length,
        ),
      ),
    );
    const batch = await command("entity-change-batch-30", {
      operation: "batch",
      agent: leadId,
      count: 30,
    });
    assert.equal(batch.ok, true, JSON.stringify(batch));
    try {
      await waitFor(
        () =>
          pulls.every((rows, index) =>
            rows
              .slice(batchPulls[index])
              .some((pull) =>
                pull.documents.some((row) => row.seq >= batch.seq),
              ),
          ),
        "both tabs did not pull the final sequence from the 30-commit burst",
      );
    } catch {
      // The base has no commit publisher; retain zero-event counts in its red run.
    }
    const afterBatchFrames = await Promise.all(
      pages.map((page) =>
        page.evaluate(
          () =>
            window.__entityFrames.filter(({ event }) =>
              event.resources.some((resource) => resource.kind === "state"),
            ).length,
        ),
      ),
    );
    const batchCounts = {
      commits: 30,
      frames: afterBatchFrames.map(
        (count, index) => count - batchFrames[index],
      ),
      pulls: pulls.map((rows, index) => rows.length - batchPulls[index]),
    };
    console.log(`ENTITY_BATCH_30 ${JSON.stringify(batchCounts)}`);
    for (let tab = 0; tab < pages.length; tab++) {
      assert.ok(
        batchCounts.frames[tab] >= 1,
        `tab ${tab} should receive a publication for the 30-commit burst`,
      );
      assert.ok(
        batchCounts.pulls[tab] >= 1 &&
          batchCounts.pulls[tab] <= batchCounts.frames[tab],
        `tab ${tab} should coalesce pulls without missing state publications`,
      );
      const deliveredRows = pulls[tab]
        .slice(batchPulls[tab])
        .flatMap((pull) => pull.documents);
      assert.ok(deliveredRows.length > 0);
      assert.equal(
        Math.max(...deliveredRows.map((row) => row.seq)),
        batch.seq,
        `tab ${tab} must pull the final entity sequence`,
      );
    }
    // Browser scheduling varies under load, so publication counts are not
    // capped here. The deterministic cap is proven by
    // test_entity_commit_publisher.py::test_scheduler_coalesces_with_a_controlled_clock.
    await waitFor(
      () =>
        statePullRequestCounts.every(
          (count, tab) => count === pulls[tab].length,
        ),
      "every state pull response must be observed before assertions",
    );
    for (let tab = 0; tab < pulls.length; tab++) {
      assert.ok(
        pulls[tab].every((pull) => pull.documents.length > 0),
        `every state pull in tab ${tab} must contain changed entity rows`,
      );
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
    await Promise.all(contexts.map((context) => context.close()));
    fixture.kill("SIGTERM");
  }
});
