import { installEntityPropagationProbe } from "./entity-commit-propagation.fixture.ts";
import {
  legacyMutationEntities,
  fixtureProtocol,
  entityRows,
  fixtureItem,
} from "./entity-fixture-protocol.ts";
import type { Page, Request } from "@playwright/test";
import type { SyncPullResponse, SyncEntity } from "../playwright.ts";

import type { PostResult } from "../../src/apiTypes.ts";
// Real renderer tabs exercise every direct sync entity writer through the
// common SQLite post-commit StateResource invalidation.
import assert from "node:assert/strict";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { performance } from "node:perf_hooks";
import {
  readTestState,
  spawnFixture,
  test,
  rustUiFixture,
} from "../playwright.ts";

type PullStart = {
  at: number;
  after: string | null;
  reset: boolean;
  fresh: boolean;
};
type PullRecord = {
  at: number;
  startedAt: number;
  after: string | null | undefined;
  reset: boolean;
  fresh: boolean;
  documents: SyncEntity[];
  checkpoint: number | undefined;
  maxSeq: number | null | undefined;
};

type ChatObservations = {
  mutationEntity: { id: string; seq: number; deleted: boolean };
  entitySequence: number;
  deliveredSequences: number[];
  responseReceivedAt: number;
  responseDecodedAt: number;
  persisterDoneAt: number;
  persisterAdvanced: boolean;
  persisterCompletion: import("./entity-commit-propagation.fixture.ts").PersisterCompletion;
  collection: string;
  deleted: boolean;
};
const timestamp = () => performance.timeOrigin + performance.now();

test("committed entities propagate between renderer tabs @sync", async ({
  browser,
}) => {
  test.setTimeout(120_000);
  const state = await mkdtemp(join(tmpdir(), "entity-commit-propagation-"));
  const fixture = spawnFixture(process.execPath, [rustUiFixture, state], {
    stdio: ["pipe", "pipe", "pipe"],
    env: { ...process.env },
  });
  const protocol = fixtureProtocol(fixture);
  const port = await protocol.port;
  const command = protocol.command;
  const origin = `http://127.0.0.1:${port}`;
  const contexts = [
    await browser.newContext(),
    await browser.newContext(),
    await browser.newContext(),
  ];
  const pages: Page[] = [];
  const pulls = contexts.map((): PullRecord[] => []);
  const statePullRequestCounts = contexts.map(() => 0);
  const pullStarts = contexts.map(() => new WeakMap<Request, PullStart>());
  const waitFor = async (
    predicate: () => unknown | Promise<unknown>,
    message: string,
    timeoutMs = 8000,
  ) => {
    const end = Date.now() + timeoutMs;
    while (Date.now() < end) {
      if (await predicate()) return;
      await new Promise((resolve) => setTimeout(resolve, 40));
    }
    assert.fail(message);
  };
  try {
    for (let index = 0; index < contexts.length; index++) {
      const page = await fixtureItem(contexts, index).newPage();
      page.setDefaultTimeout(10000);
      page.on("request", (request) => {
        const url = new URL(request.url());
        if (
          url.pathname === "/api/sync/pull" &&
          url.searchParams.get("scope") === "state:entities:v1"
        ) {
          statePullRequestCounts[index] =
            fixtureItem(statePullRequestCounts, index) + 1;
          fixtureItem(pullStarts, index).set(request, {
            at: timestamp(),
            after: url.searchParams.get("after"),
            reset: url.searchParams.has("reset"),
            fresh: url.searchParams.has("fresh"),
          });
        }
      });
      await page.addInitScript(installEntityPropagationProbe);
      page.on("response", async (response) => {
        const url = new URL(response.url());
        if (
          url.pathname !== "/api/sync/pull" ||
          url.searchParams.get("scope") !== "state:entities:v1"
        )
          return;
        try {
          const body: SyncPullResponse = await response.json();
          fixtureItem(pulls, index).push({
            at: timestamp(),
            startedAt:
              fixtureItem(pullStarts, index).get(response.request())?.at ??
              timestamp(),
            after: fixtureItem(pullStarts, index).get(response.request())
              ?.after,
            reset:
              fixtureItem(pullStarts, index).get(response.request())?.reset ??
              false,
            fresh:
              fixtureItem(pullStarts, index).get(response.request())?.fresh ??
              false,
            documents: entityRows(body.documents || []),
            checkpoint: body.checkpoint?.seq,
            maxSeq: body.maxSeq,
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
    assert.ok(lead);
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
    const results: Array<{
      elapsedMs: number;
      staleAfterMs?: number;
      [key: string]: unknown;
    }> = [];
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
      let reply: Pick<
        Awaited<ReturnType<typeof command>>,
        "ok" | "seq" | "entityId"
      > &
        Partial<ChatObservations>;
      if (params.operation === "chat") {
        await fixtureItem(pages, 0)
          .locator(".project-tree-heading")
          .first()
          .hover();
        const creationResponse = fixtureItem(pages, 0).waitForResponse(
          (response) =>
            response.request().method() === "POST" &&
            new URL(response.url()).pathname === "/api/leads",
        );
        await fixtureItem(pages, 0)
          .getByRole("button", { name: /^New chat in / })
          .first()
          .click();
        const newChatDialog = fixtureItem(pages, 0).getByRole("dialog", {
          name: "New chat",
          exact: true,
        });
        await newChatDialog
          .getByRole("button", { name: "Start chat", exact: true })
          .click();
        const created = await creationResponse;
        const responseReceivedAt = timestamp();
        const createdBody: PostResult<"/api/leads"> = await created.json();
        assert.ok("id" in createdBody && typeof createdBody.id === "string");
        const mutationEntities = legacyMutationEntities(createdBody);
        assert.equal(created.status(), 200);
        assert.ok(mutationEntities?.length);
        const createdEntity = mutationEntities.find(
          (document) =>
            document.collection === "agent" && document.id === createdBody.id,
        );
        assert.ok(
          createdEntity,
          "create response must include the new agent row",
        );
        const deliveredSequences = mutationEntities.map((doc) => doc.seq);
        await fixtureItem(pages, 0).waitForFunction(
          (sequences) =>
            window.__entityPersisterCompletions.some((completion) =>
              sequences.every((sequence) =>
                completion.sequences.includes(sequence),
              ),
            ),
          deliveredSequences,
          { timeout: 8000 },
        );
        const persisterDoneAt = await fixtureItem(pages, 0).evaluate(
          (sequences) =>
            window.__entityPersisterCompletions.find((completion) =>
              sequences.every((sequence) =>
                completion.sequences.includes(sequence),
              ),
            )?.at,
          deliveredSequences,
        );
        assert.equal(typeof persisterDoneAt, "number");
        const persisterCompletion = await fixtureItem(pages, 0).evaluate(
          (sequences) =>
            window.__entityPersisterCompletions.find((completion) =>
              sequences.every((sequence) =>
                completion.sequences.includes(sequence),
              ),
            ),
          deliveredSequences,
        );
        const responseDecodedAt = await fixtureItem(pages, 0).evaluate(
          (sequences) =>
            window.__entityMutationDecodes.find((decode) =>
              sequences.every((sequence) =>
                decode.sequences.includes(sequence),
              ),
            )?.at,
          deliveredSequences,
        );
        assert.ok(typeof responseDecodedAt === "number");
        assert.ok(typeof persisterDoneAt === "number");
        assert.ok(persisterCompletion);
        await fixtureItem(pages, 0)
          .locator("#conversation-title")
          .getByText("New chat", { exact: true })
          .waitFor();
        reply = {
          ok: true,
          entityId: createdBody.id,
          seq: Math.max(...mutationEntities.map((doc) => doc.seq)),
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
            deleted: createdEntity.op === "delete",
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
                              `entity:${row.collection}:${row.id}` ===
                                documentId &&
                              row.seq === seq &&
                              (row.op === "delete") === deleted,
                          ),
                      ),
                    ),
                {
                  from: fixtureItem(previousFrames, tab),
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
      const observed = await fixtureItem(pages, observedTab).evaluate(
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
                (row) =>
                  `entity:${row.collection}:${row.id}` === documentId &&
                  row.seq === seq,
              );
              if (document) return { at: frame.at, document };
            }
          }
          return null;
        },
        {
          from: fixtureItem(previousFrames, observedTab),
          documentId: entityDocumentId,
          seq: expectedEntitySequence,
        },
      );
      assert.ok(observed, `${params.operation} pushed row must be observable`);
      const document = observed.document;
      if (params.operation === "chat") {
        await fixtureItem(pages, 0)
          .locator(`[data-chat="${entityId}"]`)
          .waitFor();
        await fixtureItem(pages, 1)
          .locator(`[data-chat="${entityId}"]`)
          .waitFor();
        await fixtureItem(pages, 2)
          .locator(`[data-chat="${entityId}"]`)
          .waitFor();
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
            fixtureItem(previousFrames, tab),
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
        (rows, tab) => rows.length - fixtureItem(previousPulls, tab),
      );
      console.log(
        `ENTITY_OPERATION_COUNTS ${JSON.stringify({ operation: params.operation, frames: operationFrameCounts, pulls: operationPullCounts })}`,
      );
      assert.ok(
        operationPullCounts.every(
          (count, tab) => count <= fixtureItem(operationFrameCounts, tab),
        ),
        `${params.operation} must not pull more times than it publishes state`,
      );
      if (params.operation === "chat") {
        assert.ok(
          reply.deliveredSequences &&
            reply.persisterDoneAt !== undefined &&
            reply.responseDecodedAt !== undefined,
        );
        const persisterDoneAt = reply.persisterDoneAt;
        const responseDecodedAt = reply.responseDecodedAt;
        const responseSequences = new Set(reply.deliveredSequences);
        const actorFramesAfterMutation = fixtureItem(operationFrames, 0);
        const actorPullTimings = fixtureItem(pulls, 0)
          .slice(fixtureItem(previousPulls, 0))
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
        const actorDuplicatePulls = fixtureItem(pulls, 0)
          .slice(fixtureItem(previousPulls, 0))
          .filter((pull) =>
            pull.documents.some((row) => responseSequences.has(row.seq)),
          );
        const actorPrePersisterPulls = actorDuplicatePulls.filter(
          (pull) => pull.startedAt <= persisterDoneAt,
        );
        const actorPostPersisterPulls = actorDuplicatePulls.filter(
          (pull) => pull.startedAt > persisterDoneAt,
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
              (pull) => pull.startedAt <= responseDecodedAt,
            ).length,
            afterResponseDecode: actorDuplicatePulls.filter(
              (pull) => pull.startedAt > responseDecodedAt,
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
            return !!precedingFrame && precedingFrame.at < responseDecodedAt;
          }),
          `every remaining duplicate must be triggered by a frame handled before response decoding: ${JSON.stringify(duplicateCauses)}`,
        );
      }
      if (params.operation === "rename") {
        assert.ok(document.collection === "agent" && document.op === "upsert");
        assert.equal(document.value.name, params.name);
      }
      if (params.operation === "budget") {
        assert.ok(document.collection === "agent" && document.op === "upsert");
        assert.ok(Number(document.value.tokensUsed) >= 13);
      }
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
            { from: fixtureItem(previousFrames, tab), startedAt },
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
              frameArrivals: fixtureItem(changedFrames, 0).map(
                ({ elapsedMs }) => startedAt + elapsedMs,
              ),
              actorPullStarts: fixtureItem(pulls, 0)
                .slice(fixtureItem(previousPulls, 0))
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
        pulls: pulls.map(
          (rows, tab) => rows.length - fixtureItem(previousPulls, tab),
        ),
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
    assert.ok(batchLead);
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
                        "state" &&
                      (version.entityChanges?.through ?? Number.NaN) >= seq,
                  ),
                ),
            { from: fixtureItem(batchFrameOffsets, tab), seq: batch.seq },
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
        (count, index) => count - fixtureItem(batchFrames, index),
      ),
      pulls: pulls.map(
        (rows, index) => rows.length - fixtureItem(batchPulls, index),
      ),
    };
    console.log(`ENTITY_BATCH_30 ${JSON.stringify(batchCounts)}`);
    for (let tab = 0; tab < pages.length; tab++) {
      assert.ok(
        fixtureItem(batchCounts.frames, tab) >= 1,
        `tab ${tab} should receive a publication for the 30-commit burst`,
      );
      const delivered = await fixtureItem(pages, tab).evaluate(
        ({ from, leadId, seq }) => {
          for (const { event } of window.__entityFrames.slice(from)) {
            for (const [index, version] of (
              event.resourceVersions || []
            ).entries()) {
              if (
                (version.resource ?? event.resources[index])?.kind !==
                  "state" ||
                (version.entityChanges?.through ?? Number.NaN) < seq
              )
                continue;
              const document = version.entityChanges?.documents.find(
                (row) =>
                  row.collection === "agent" &&
                  row.id === leadId &&
                  row.seq === seq,
              );
              if (document)
                return {
                  seq: document.seq,
                  value: "value" in document ? document.value : undefined,
                };
            }
          }
          return null;
        },
        { from: fixtureItem(batchFrameOffsets, tab), leadId, seq: batch.seq },
      );
      assert.equal(
        delivered?.seq,
        batch.seq,
        `tab ${tab} must persist the final entity sequence from its stream frame`,
      );
      assert.equal(
        Number(
          delivered?.value && "tokensUsed" in delivered.value
            ? delivered.value.tokensUsed
            : undefined,
        ),
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
          (count, tab) => count === fixtureItem(pulls, tab).length,
        ),
      "every state pull response must be observed before assertions",
    );
    for (let tab = 0; tab < pulls.length; tab++) {
      for (const pull of fixtureItem(pulls, tab)) {
        const bootstrap = Number(pull.after) === 0 || pull.reset || pull.fresh;
        assert.ok(
          pull.documents.length > 0,
          `every state pull in tab ${tab} must contain changed entity rows${bootstrap ? " (bootstrap)" : ""}: ${JSON.stringify(pull)}`,
        );
      }
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
