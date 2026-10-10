// Real renderer tabs exercise every direct sync entity writer through the
// common SQLite post-commit StateResource invalidation.
import assert from "node:assert/strict";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { performance } from "node:perf_hooks";
import { readTestState, spawnFixture, test } from "../playwright.mjs";

const timestamp = () => performance.timeOrigin + performance.now();

test("committed entities propagate between renderer tabs @sync", async ({
  browser,
}) => {
  test.setTimeout(120_000);
  const repo = fileURLToPath(new URL("../../../../../../", import.meta.url));
  const state = await mkdtemp(join(tmpdir(), "entity-commit-propagation-"));
  const fixture = spawnFixture(
    "python3",
    [
      "-B",
      join(repo, "workspaces/runtime/apps/server/tests/simple-ui-fixture.py"),
      state,
    ],
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
  const origin = `http://127.0.0.1:${port}`;
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
      if (await predicate()) return;
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
        window.__entityStreamAttempts = [];
        window.__entityPersisterCompletions = [];
        window.__entityMutationDecodes = [];
        const responseJson = Response.prototype.json;
        Response.prototype.json = async function (...args) {
          const value = await responseJson.apply(this, args);
          if (
            this.url &&
            new URL(this.url).pathname === "/api/leads" &&
            value?._syncEntities?.length
          )
            window.__entityMutationDecodes.push({
              at: performance.timeOrigin + performance.now(),
              sequences: value._syncEntities.map((row) => row.seq),
            });
          return value;
        };
        window.__studioSyncEntityPersisterProbe = (value) =>
          window.__entityPersisterCompletions.push(value);
        window.EventSource = class extends OriginalEventSource {
          constructor(...args) {
            super(...args);
            const streamIndex = window.__entityStreamUrls.length;
            const url = String(args[0]);
            const attempt = { url, opened: false, errored: false };
            window.__entityStreamUrls.push(url);
            window.__entityStreamAttempts.push(attempt);
            this.addEventListener("open", () => (attempt.opened = true));
            this.addEventListener("error", () => (attempt.errored = true));
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
      await page.goto(origin);
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
    const currentState = await readTestState(origin);
    const lead = currentState.threads.find(
      (agent) => agent.name === "Release lead",
    );
    const leadId = lead.id;
    await waitFor(
      () =>
        Promise.all(
          pages.map((page) =>
            page.evaluate((expectedLeadId) => {
              return (
                // Subscription updates are coalesced. Verify the current
                // stream's resource coverage instead of requiring one URL per
                // shell, drafts, foreground, and opened-chat stage.
                window.__entityStreamUrls.length > 0 &&
                window.__entityFrames.some(
                  ({ event }) =>
                    event.reason === "initial" &&
                    event.resources.some(
                      (resource) => resource.kind === "state",
                    ) &&
                    event.resources.some(
                      (resource) => resource.kind === "drafts",
                    ) &&
                    event.resources.some(
                      (resource) =>
                        resource.kind === "transcript" &&
                        resource.agentId === expectedLeadId,
                    ),
                )
              );
            }, leadId),
          ),
        ).then((ready) => ready.every(Boolean)),
      "the coalesced stream did not include state, drafts, and the opened chat transcript in every tab",
    );
    for (const page of pages) {
      const attempts = await page.evaluate(() => window.__entityStreamAttempts);
      const liveReopens = attempts.filter((attempt, index) => {
        if (index === 0) return false;
        const previousAttempt = attempts
          .slice(0, index)
          .filter((prior) => prior.url === attempt.url)
          .at(-1);
        return previousAttempt?.opened && !previousAttempt.errored;
      });
      assert.deepEqual(
        liveReopens,
        [],
        `a successfully opened stream URL was reopened before error: ${JSON.stringify(liveReopens)}`,
      );
    }
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
        const createdEntity = createdBody._syncEntities.find(
          (document) => document.id === `entity:agent:${createdBody.id}`,
        );
        assert.ok(
          createdEntity,
          "create response must include the new agent row",
        );
        const deliveredSequences = createdBody._syncEntities.map(
          (doc) => doc.seq,
        );
        await pages[0].waitForFunction(
          (sequences) =>
            window.__entityPersisterCompletions.some((completion) =>
              sequences.every((sequence) =>
                completion.sequences.includes(sequence),
              ),
            ),
          deliveredSequences,
          { timeout: 8000 },
        );
        const persisterDoneAt = await pages[0].evaluate(
          (sequences) =>
            window.__entityPersisterCompletions.find((completion) =>
              sequences.every((sequence) =>
                completion.sequences.includes(sequence),
              ),
            )?.at,
          deliveredSequences,
        );
        assert.equal(typeof persisterDoneAt, "number");
        const persisterCompletion = await pages[0].evaluate(
          (sequences) =>
            window.__entityPersisterCompletions.find((completion) =>
              sequences.every((sequence) =>
                completion.sequences.includes(sequence),
              ),
            ),
          deliveredSequences,
        );
        const responseDecodedAt = await pages[0].evaluate(
          (sequences) =>
            window.__entityMutationDecodes.find((decode) =>
              sequences.every((sequence) =>
                decode.sequences.includes(sequence),
              ),
            )?.at,
          deliveredSequences,
        );
        assert.equal(typeof responseDecodedAt, "number");
        await pages[0]
          .locator("#conversation-title")
          .getByText("New chat", { exact: true })
          .waitFor();
        reply = {
          ok: true,
          entityId: createdBody.id,
          seq: Math.max(...createdBody._syncEntities.map((doc) => doc.seq)),
          entitySequence: createdEntity.seq,
          deliveredSequences,
          responseReceivedAt,
          responseDecodedAt,
          persisterDoneAt,
          persisterAdvanced: persisterCompletion.advanced,
          persisterCompletion,
          mutationEntity: {
            id: createdEntity.id,
            seq: createdEntity.seq,
            deleted: createdEntity._deleted ?? false,
          },
          collection: "agent",
          deleted: false,
        };
      } else {
        reply = await command(commandId, params);
      }
      assert.equal(reply.ok, true, JSON.stringify(reply));
      const entityId = reply.entityId;
      const expectedEntitySequence = reply.entitySequence ?? reply.seq;
      const entityDocumentId = `entity:${params.collection}:${entityId}`;
      // The actor gets a new chat's row in its mutation response; peers get it
      // through their state stream.
      if (params.operation === "chat") {
        assert.deepEqual(reply.mutationEntity, {
          id: entityDocumentId,
          seq: expectedEntitySequence,
          deleted: false,
        });
      }
      await waitFor(
        () =>
          Promise.all(
            pages.map(async (page, tab) => {
              if (tab === 0 && params.operation === "chat") return true;
              return page.evaluate(
                ({ from, documentId, seq, deleted }) =>
                  window.__entityFrames
                    .slice(from)
                    .some(({ event }) =>
                      event.resourceVersions?.some(
                        (version, index) =>
                          (version.resource ?? event.resources[index])?.kind ===
                            "state" &&
                          version.entityChanges?.documents.some(
                            (row) =>
                              row.id === documentId &&
                              row.seq === seq &&
                              row._deleted === deleted,
                          ),
                      ),
                    ),
                {
                  from: previousFrames[tab],
                  documentId: entityDocumentId,
                  seq: expectedEntitySequence,
                  deleted: params.deleted,
                },
              );
            }),
          ).then((received) => received.every(Boolean)),
        `${params.operation} did not publish its exact entity row to every tab`,
      );
      await waitFor(
        () =>
          Promise.all(
            pages.map((page) =>
              page.evaluate(
                (seq) =>
                  window.__entityPersisterCompletions.some(
                    (completion) =>
                      completion.sequences.includes(seq) &&
                      completion.checkpoint >= seq,
                  ),
                expectedEntitySequence,
              ),
            ),
          ).then((persisted) => persisted.every(Boolean)),
        `${params.operation} entity row was not persisted on every tab`,
      );
      const observedTab = params.operation === "chat" ? 1 : 0;
      const observed = await pages[observedTab].evaluate(
        ({ from, documentId, seq }) => {
          for (const frame of window.__entityFrames.slice(from)) {
            for (const [index, version] of (
              frame.event.resourceVersions || []
            ).entries()) {
              if (
                (version.resource ?? frame.event.resources[index])?.kind !==
                "state"
              )
                continue;
              const document = version.entityChanges?.documents.find(
                (row) => row.id === documentId && row.seq === seq,
              );
              if (document) return { at: frame.at, document };
            }
          }
          return null;
        },
        {
          from: previousFrames[observedTab],
          documentId: entityDocumentId,
          seq: expectedEntitySequence,
        },
      );
      assert.ok(observed, `${params.operation} pushed row must be observable`);
      const document = observed.document;
      if (params.operation === "chat") {
        await pages[0].locator(`[data-chat="${entityId}"]`).waitFor();
        await pages[1].locator(`[data-chat="${entityId}"]`).waitFor();
        await pages[2].locator(`[data-chat="${entityId}"]`).waitFor();
      }
      // Attribute the delayed stream-reopen baseline to this operation before
      // starting the next one. A coalesced baseline can arrive after the
      // mutation's pushed entity row was already persisted.
      await new Promise((resolve) => setTimeout(resolve, 600));
      const operationFrames = await Promise.all(
        pages.map((page, tab) =>
          page.evaluate(
            (from) =>
              window.__entityFrames.slice(from).map(({ at, event }) => ({
                at,
                reason: event.reason,
                resources: event.resources,
                resourceVersions: event.resourceVersions,
              })),
            previousFrames[tab],
          ),
        ),
      );
      const operationFrameCounts = operationFrames.map(
        (frames) =>
          frames.filter((frame) =>
            frame.resources.some((resource) => resource.kind === "state"),
          ).length,
      );
      const operationPullCounts = pulls.map(
        (rows, tab) => rows.length - previousPulls[tab],
      );
      console.log(
        `ENTITY_OPERATION_COUNTS ${JSON.stringify({ operation: params.operation, frames: operationFrameCounts, pulls: operationPullCounts })}`,
      );
      assert.ok(
        operationPullCounts.every(
          (count, tab) => count <= operationFrameCounts[tab],
        ),
        `${params.operation} must not pull more times than it publishes state`,
      );
      if (params.operation === "chat") {
        const responseSequences = new Set(reply.deliveredSequences);
        const actorFramesAfterMutation = operationFrames[0];
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
            responseDecodedAt: reply.responseDecodedAt,
            persisterDoneAt: reply.persisterDoneAt,
            persisterAdvanced: reply.persisterAdvanced,
            persisterCompletion: reply.persisterCompletion,
            frameArrivals: actorFramesAfterMutation,
            actorPulls: actorPullTimings,
          })}`,
        );
        const actorDuplicatePulls = pulls[0]
          .slice(previousPulls[0])
          .filter((pull) =>
            pull.documents.some((row) => responseSequences.has(row.seq)),
          );
        const actorPrePersisterPulls = actorDuplicatePulls.filter(
          (pull) => pull.startedAt <= reply.persisterDoneAt,
        );
        const actorPostPersisterPulls = actorDuplicatePulls.filter(
          (pull) => pull.startedAt > reply.persisterDoneAt,
        );
        const duplicateCauses = actorDuplicatePulls.map((pull) => {
          const precedingStateFrame = actorFramesAfterMutation
            .filter(
              (frame) =>
                frame.at <= pull.startedAt &&
                frame.resources.some((resource) => resource.kind === "state"),
            )
            .at(-1);
          const stateIndex = precedingStateFrame?.resources.findIndex(
            (resource) => resource.kind === "state",
          );
          const stateVersion =
            precedingStateFrame?.resourceVersions?.find(
              (version) => version.resource?.kind === "state",
            ) ??
            (stateIndex === undefined || stateIndex < 0
              ? undefined
              : precedingStateFrame?.resourceVersions?.[stateIndex]);
          return {
            startedAt: pull.startedAt,
            after: pull.after,
            sequences: pull.documents.map((row) => row.seq),
            precedingFrame: precedingStateFrame
              ? {
                  at: precedingStateFrame.at,
                  reason: precedingStateFrame.reason,
                  entitySequences: stateVersion?.entitySequences,
                }
              : null,
          };
        });
        console.log(
          `ENTITY_ACTOR_DUPLICATE_PULLS ${JSON.stringify({
            count: actorDuplicatePulls.length,
            beforePersisterDone: actorPrePersisterPulls.length,
            afterPersisterDone: actorPostPersisterPulls.length,
            beforeResponseDecode: actorDuplicatePulls.filter(
              (pull) => pull.startedAt <= reply.responseDecodedAt,
            ).length,
            afterResponseDecode: actorDuplicatePulls.filter(
              (pull) => pull.startedAt > reply.responseDecodedAt,
            ).length,
            causes: duplicateCauses,
            responseDecodedAt: reply.responseDecodedAt,
            responseSequences: [...responseSequences],
          })}`,
        );
        assert.equal(
          actorPostPersisterPulls.length,
          0,
          `create-chat actor pulls starting after the persister completed must not return rows already delivered in the mutation response: ${JSON.stringify(duplicateCauses)}`,
        );
        assert.ok(
          actorDuplicatePulls.every((pull) => {
            const precedingFrame = actorFramesAfterMutation
              .filter(
                (frame) =>
                  frame.at <= pull.startedAt &&
                  frame.resources.some((resource) => resource.kind === "state"),
              )
              .at(-1);
            return (
              !!precedingFrame && precedingFrame.at < reply.responseDecodedAt
            );
          }),
          `every remaining duplicate must be triggered by a frame handled before response decoding: ${JSON.stringify(duplicateCauses)}`,
        );
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
    const batchFrameOffsets = await Promise.all(
      pages.map((page) => page.evaluate(() => window.__entityFrames.length)),
    );
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
    const batchState = await readTestState(origin);
    const batchLead = batchState.threads.find((agent) => agent.id === leadId);
    const batchBeforeTokensUsed = Number(batchLead.tokensUsed || 0);
    const batch = await command("entity-change-batch-30", {
      operation: "batch",
      agent: leadId,
      count: 30,
    });
    assert.equal(batch.ok, true, JSON.stringify(batch));
    await waitFor(async () => {
      const frames = await Promise.all(
        pages.map((page, tab) =>
          page.evaluate(
            ({ from, seq }) =>
              window.__entityFrames
                .slice(from)
                .some(({ event }) =>
                  event.resourceVersions?.some(
                    (version, index) =>
                      (version.resource ?? event.resources[index])?.kind ===
                        "state" && version.entityChanges?.through >= seq,
                  ),
                ),
            { from: batchFrameOffsets[tab], seq: batch.seq },
          ),
        ),
      );
      return frames.every(Boolean);
    }, "all tabs did not receive the pushed final entity sequence");
    await waitFor(
      () =>
        Promise.all(
          pages.map((page) =>
            page.evaluate(
              (seq) =>
                window.__entityPersisterCompletions.some(
                  (completion) =>
                    completion.sequences.includes(seq) &&
                    completion.checkpoint >= seq,
                ),
              batch.seq,
            ),
          ),
        ).then((persisted) => persisted.every(Boolean)),
      "all tabs did not persist the pushed final entity sequence",
    );
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
      const delivered = await pages[tab].evaluate(
        ({ from, leadId, seq }) => {
          for (const { event } of window.__entityFrames.slice(from)) {
            for (const [index, version] of (
              event.resourceVersions || []
            ).entries()) {
              if (
                (version.resource ?? event.resources[index])?.kind !==
                  "state" ||
                version.entityChanges?.through < seq
              )
                continue;
              const document = version.entityChanges.documents.find(
                (row) => row.id === `entity:agent:${leadId}` && row.seq === seq,
              );
              if (document)
                return {
                  seq: document.seq,
                  value: JSON.parse(document.payload).value,
                };
            }
          }
          return null;
        },
        { from: batchFrameOffsets[tab], leadId, seq: batch.seq },
      );
      assert.equal(
        delivered?.seq,
        batch.seq,
        `tab ${tab} must persist the final entity sequence from its stream frame`,
      );
      assert.equal(
        Number(delivered?.value.tokensUsed),
        batchBeforeTokensUsed + 30,
        `tab ${tab} must persist all thirty committed token increments`,
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
