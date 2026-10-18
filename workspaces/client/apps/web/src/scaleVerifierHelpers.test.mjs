import { mkdirSync, mkdtempSync, writeFileSync } from "node:fs";
import { spawn, spawnSync } from "node:child_process";
import { randomUUID } from "node:crypto";
import {
  mkdir,
  mkdtemp,
  readFile,
  readdir,
  rename,
  rm,
  stat,
  symlink,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { performance } from "node:perf_hooks";
import { fileURLToPath } from "node:url";
import { afterEach, describe, expect, it } from "vitest";
import {
  DURABLE_INPUT_QUEUE_KEY,
  PARENT_NOTICE_INPUT_KIND,
  RETAINED_INPUT_STATUSES,
  SCALE_PROVIDER,
  scaleAccountKeys,
  releaseBarriersNow,
  readProcessTable,
  sampleProviderProcesses,
  scaleProfile,
  SCALE_WORKSPACE_MODE,
  TOOL_BARRIER_PROCESS_BOUND_MS,
  acquireRunLock,
  assertConcurrentWorkers,
  assertServedModelEvidence,
  assertStoredScaleProfile,
  callbackProcessRetirementDelayMs,
  callbackWorktreeCwd,
  canonicalJournalBootId,
  classifyRetainedInputs,
  classifyStageQueues,
  createBoundedEventQueue,
  createEntityCursorPuller,
  createReadinessMarkerDirectory,
  createStateResourceAdapter,
  findFifoHolders,
  finishedWorkspaceCleanupIds,
  foreignLiveTeamRows,
  forEachBounded,
  normalizedWorktreePath,
  parseProviderStdinJournalMessage,
  parseWorkerWorkspace,
  releaseFifoBarriers,
  requestIdentityKey,
  requireScaleModel,
  resumeIntentState,
  scaleWorkerDefaults,
  sharedBarrierPaths,
  watchReadinessMarkers,
  waitForServedModelEvidence,
} from "../scripts/scaleVerifierHelpers.mjs";
import {
  ENTITY_BASELINE_PULL_PATH,
  parseStages,
  readCurrentTeamChildren,
  readRetainedInputRows,
  readServedModelRecords,
} from "../scripts/verify-rust-chat-scale.mjs";

const temporaryPaths = [];

it("waits for a publication event before rereading missing served-model evidence", async () => {
  let reads = 0;
  let publish;
  let subscribed;
  const registered = new Promise((resolve) => {
    subscribed = resolve;
  });
  const wait = waitForServedModelEvidence({
    agentIds: ["owned"],
    minimumResponses: 1,
    readRecords: () => {
      reads += 1;
      return reads === 1
        ? new Map()
        : new Map([
            ["owned", [{ turnId: "turn", model: "claude-haiku-fixture" }]],
          ]);
    },
    waitForPublication: () =>
      new Promise((resolve) => {
        publish = resolve;
        subscribed();
      }),
  });
  await registered;
  expect(reads).toBe(1);
  publish();
  expect((await wait).get("owned")).toHaveLength(1);
  expect(reads).toBe(2);
});

it("fails closed on a non-Haiku response and on an expired publication wait", async () => {
  const options = { agentIds: ["owned"], minimumResponses: 2 };
  await expect(
    waitForServedModelEvidence({
      ...options,
      readRecords: () =>
        new Map([["owned", [{ turnId: "turn", model: "other-model" }]]]),
      waitForPublication: () => {
        throw new Error("must not wait for the wrong model");
      },
    }),
  ).rejects.toThrow(/requires claude-haiku-\*/);
  await expect(
    waitForServedModelEvidence({
      ...options,
      readRecords: () => new Map(),
      waitForPublication: () => {
        throw new Error("publication deadline");
      },
    }),
  ).rejects.toThrow(/publication deadline/);
});

it("joins the Linux and journald representations of one boot identity", () => {
  const kernel = "1dcf74a6-6f54-45d7-af49-a68206aa72f0";
  const journal = "1dcf74a66f5445d7af49a68206aa72f0";
  expect(canonicalJournalBootId(kernel)).toBe(journal);
  expect(canonicalJournalBootId(journal)).toBe(journal);
  expect(canonicalJournalBootId(kernel.toUpperCase())).toBe(journal);
  for (const invalid of [
    undefined,
    "",
    `${kernel}-`,
    `-${journal}`,
    journal.slice(1),
    "x".repeat(32),
  ])
    expect(() => canonicalJournalBootId(invalid)).toThrow(/boot ID/);
});

afterEach(async () => {
  await Promise.all(
    temporaryPaths
      .splice(0)
      .map((path) => rm(path, { recursive: true, force: true })),
  );
});

describe("scale verifier helpers", () => {
  it("uses the public worktree projection after the callback readiness signal", () => {
    const entity = {
      cwd: "/project/.worktrees/codex-agents/worker",
      workspaceMode: SCALE_WORKSPACE_MODE,
      worktree: true,
    };
    expect(callbackWorktreeCwd(entity, "worker", "/project")).toBe(entity.cwd);
    for (const changes of [
      { worktree: false },
      { workspaceMode: "shared" },
      { cwd: "/project" },
      { cwd: "relative" },
      { worktreePreparation: "waiting" },
      { worktreePreparation: "preparing" },
    ])
      expect(() =>
        callbackWorktreeCwd({ ...entity, ...changes }, "worker", "/project"),
      ).toThrow();
  });

  it("normalizes an absolute worktree path without accepting relative identities", () => {
    expect(normalizedWorktreePath("/owned/worktree/")).toBe("/owned/worktree");
    expect(normalizedWorktreePath("/owned/another")).not.toBe(
      "/owned/worktree",
    );
    expect(() => normalizedWorktreePath("owned/worktree")).toThrow(/absolute/);
    expect(() => normalizedWorktreePath(null)).toThrow(/absolute/);
  });

  it("keeps workspace selection outside the worker execution defaults", () => {
    expect(
      scaleWorkerDefaults("claude-haiku-fixture", SCALE_PROVIDER, [
        "claude-local",
      ]),
    ).toEqual({
      account_key: "claude-local",
      model: "claude-haiku-fixture",
      effort: "low",
      fast_mode: false,
      delegation_depth: 1,
    });
  });
  it("uses the server's page size for the initial entity checkpoint", () => {
    const url = new URL(ENTITY_BASELINE_PULL_PATH, "http://127.0.0.1");
    expect(url.pathname).toBe("/api/sync/pull");
    expect(Object.fromEntries(url.searchParams)).toEqual({
      scope: "state:entities:v1",
      after: "0",
      fresh: "1",
      reset: "1",
    });
  });
  it("restricts paid model calls to the approved cheap Claude profile", () => {
    expect(SCALE_PROVIDER).toMatchObject({
      accountKey: "claude-local",
      model: "haiku",
      effort: "low",
      fastMode: false,
    });
    expect(Object.isFrozen(SCALE_PROVIDER)).toBe(true);
  });
  it("spreads Codex workers over ready accounts and keeps defaults unpinned", () => {
    const accounts = [
      { id: "login-b", status: "ready" },
      { id: "claude-local", status: "ready", provider: "claude" },
      { id: "default", status: "ready" },
      { id: "login-c", status: "needs-login" },
    ];
    const codex = scaleProfile("codex-luna");
    const keys = scaleAccountKeys(accounts, codex);
    expect(keys).toEqual(["default", "login-b"]);
    expect(scaleWorkerDefaults("gpt-6-luna", codex, keys)).not.toHaveProperty(
      "account_key",
    );
    expect(scaleWorkerDefaults("m", codex, ["default"]).account_key).toBe(
      "default",
    );
    expect(scaleAccountKeys(accounts, scaleProfile("claude-sonnet"))).toEqual([
      "claude-local",
    ]);
  });
  it("selects the cheapest Codex model without a resolved id", () => {
    const codex = scaleProfile("codex-luna");
    expect(codex).toMatchObject({ model: "gpt-6-luna", effort: "low" });
    const row = (model) => ({
      model,
      resolvedModel: null,
      supportedReasoningEfforts: [{ reasoningEffort: "low" }],
    });
    expect(
      requireScaleModel(
        { data: [row("gpt-6.1-sol"), row("gpt-6-luna")] },
        codex,
      ).model,
    ).toBe("gpt-6-luna");
    expect(() =>
      requireScaleModel({ data: [row("gpt-6.1-sol")] }, codex),
    ).toThrow("unavailable");
    expect(() => scaleProfile("other")).toThrow("Profile must be");
  });
  it("selects Haiku even when the account default is Opus", () => {
    const haiku = {
      model: SCALE_PROVIDER.model,
      resolvedModel: "claude-haiku-fixture",
      supportedReasoningEfforts: [{ reasoningEffort: SCALE_PROVIDER.effort }],
    };
    expect(
      requireScaleModel({
        data: [
          { model: "default", resolvedModel: "claude-opus-fixture" },
          haiku,
        ],
      }),
    ).toBe(haiku);
  });

  it("rejects missing, expensive, and unsupported model selections", () => {
    expect(() =>
      requireScaleModel({
        data: [{ model: "default", resolvedModel: "claude-opus-fixture" }],
      }),
    ).toThrow("unavailable");
    for (const resolvedModel of [
      undefined,
      "claude-opus-fixture",
      "gpt-fixture",
    ]) {
      expect(() =>
        requireScaleModel({
          data: [{ model: SCALE_PROVIDER.model, resolvedModel }],
        }),
      ).toThrow("must resolve to claude-haiku-*");
    }
    expect(() =>
      requireScaleModel({
        data: [
          {
            model: SCALE_PROVIDER.model,
            resolvedModel: "claude-haiku-fixture",
            supportedReasoningEfforts: [],
          },
        ],
      }),
    ).toThrow("does not expose");
  });

  it("parses the runtime-process-owned physical-flush journal fixture", async () => {
    const line = (
      await readFile(
        new URL(
          "../../../../runtime/packages/runtime-process/tests/fixtures/provider-stdin-timing-diagnostic.log",
          import.meta.url,
        ),
        "utf8",
      )
    ).trim();
    expect(parseProviderStdinJournalMessage(line)).toMatchObject({
      providerHandle: "account:default",
      generation: 7,
      operationId: "attempt-op",
      requestId: "request-1",
      method: "turn/start",
      stdinFlushedAtNs: 1_234_567_890,
    });
    expect(parseProviderStdinJournalMessage("unrelated service message")).toBe(
      null,
    );
  });

  it("keeps original item indexes across bounded batches", async () => {
    const seen = [];
    await forEachBounded(["a", "b", "c", "d", "e"], 2, async (value, index) => {
      seen.push([value, index]);
    });
    expect(seen).toEqual([
      ["a", 0],
      ["b", 1],
      ["c", 2],
      ["d", 3],
      ["e", 4],
    ]);
  });

  it("does not resend a prepared cleanup intent recovered from disk", () => {
    expect(resumeIntentState({ state: "prepared" }, true).state).toBe(
      "unknown",
    );
    expect(resumeIntentState({ state: "prepared" }, false).state).toBe(
      "prepared",
    );
  });

  it("exclusively owns a run journal and reclaims only a dead matching owner", async () => {
    const directory = await mkdtemp(join(tmpdir(), "scale-lock-"));
    temporaryPaths.push(directory);
    const path = join(directory, "run.lock");
    const release = await acquireRunLock(path, "run-1");
    await expect(acquireRunLock(path, "run-1")).rejects.toThrow(
      "already active",
    );
    await release();
    const stale = JSON.stringify({
      runId: "run-1",
      pid: 2_000_000_000,
      token: "old",
    });
    await writeFile(path, stale, { mode: 0o600 });
    const reclaim = await acquireRunLock(path, "run-1");
    expect(
      (await readdir(directory)).some((name) =>
        name.startsWith("run.lock.stale-"),
      ),
    ).toBe(true);
    await reclaim();
  });

  it("fails visibly instead of dropping buffered sync events", () => {
    const events = createBoundedEventQueue(1);
    expect(events.push("first")).toBe(true);
    expect(events.push("second")).toBe(false);
    expect(events.failure?.message).toMatch(/exceeded its 1 item limit/);
    expect(events.shift()).toBe("first");
  });

  it("resolves an SSE invalidation through ordered public cursor pulls", async () => {
    const requests = [];
    const pages = [
      {
        workspaceId: "workspace-a",
        checkpoint: { seq: 2 },
        documents: [
          { id: "entity:agent:worker-a", seq: 1, payload: "running" },
        ],
      },
      {
        workspaceId: "workspace-a",
        checkpoint: { seq: 4 },
        documents: [
          { id: "entity:agent:worker-a", seq: 4, payload: "completed" },
          { id: "entity:agent:worker-b", seq: 3, payload: "running" },
        ],
      },
    ];
    const pullThrough = createEntityCursorPuller(
      0,
      "workspace-a",
      async (after) => {
        requests.push(after);
        return pages.shift();
      },
    );

    // The adapter is called only after the SSE invalidation identifies seq 4.
    const observation = await pullThrough(4);

    expect(requests).toEqual([0, 2]);
    expect(observation).toMatchObject({ after: 0, through: 4 });
    expect(observation.documents).toEqual([
      { id: "entity:agent:worker-b", seq: 3, payload: "running" },
      { id: "entity:agent:worker-a", seq: 4, payload: "completed" },
    ]);
  });

  it("refreshes the public cursor on same-sequence SSE invalidations", async () => {
    const requests = [];
    const pullThrough = createEntityCursorPuller(
      7,
      "workspace-a",
      async (after) => {
        requests.push(after);
        return {
          workspaceId: "workspace-a",
          checkpoint: { seq: 8 },
          documents: [
            { id: "entity:agent:worker-a", seq: 8, payload: "cleaned" },
          ],
        };
      },
    );

    const observation = await pullThrough(7, { force: true });

    expect(requests).toEqual([7]);
    expect(observation.through).toBe(8);
    expect(observation.documents).toEqual([
      { id: "entity:agent:worker-a", seq: 8, payload: "cleaned" },
    ]);
  });

  it("maps Rust state SSE baselines and unchanged-sequence invalidations through the public cursor", async () => {
    const requests = [];
    const pages = [
      {
        workspaceId: "workspace-a",
        checkpoint: { seq: 8 },
        documents: [
          { id: "entity:agent:worker-a", seq: 8, payload: "ready" },
          { id: "entity:agent:foreign", seq: 8, payload: "ignored" },
        ],
      },
      {
        workspaceId: "workspace-a",
        checkpoint: { seq: 9 },
        documents: [
          { id: "entity:agent:worker-a", seq: 9, payload: "deleted" },
        ],
      },
    ];
    const observe = createStateResourceAdapter(
      "workspace-a",
      new Set(["worker-a"]),
      async (after) => {
        requests.push(after);
        return pages.shift();
      },
    );
    const rustFrame = (reason, revision) => ({
      reason,
      workspaceId: "workspace-a",
      resources: [{ kind: "state" }],
      resourceVersions: [{ revision, entitySequenceReset: false }],
    });

    const baseline = await observe(rustFrame("initial", 7));
    const unchanged = await observe(rustFrame("change", 7), 7);

    expect(requests).toEqual([7, 8]);
    expect(baseline.stateDocuments).toEqual([
      { id: "entity:agent:worker-a", seq: 8, payload: "ready" },
    ]);
    expect(unchanged).toMatchObject({ sequence: 9 });
    expect(unchanged.stateDocuments).toEqual([
      { id: "entity:agent:worker-a", seq: 9, payload: "deleted" },
    ]);
  });

  it("pulls broad Rust state hints without accepting a cursor reset or workspace change", async () => {
    const requests = [];
    let reset = false;
    const observe = createStateResourceAdapter(
      "workspace-a",
      new Set(["worker-a"]),
      async (after) => {
        requests.push(after);
        return {
          workspaceId: "workspace-a",
          checkpoint: { seq: 7 },
          documents: [],
          reset,
        };
      },
    );
    const frame = (reason, revision, workspaceId = "workspace-a") => ({
      reason,
      workspaceId,
      resources: [{ kind: "state" }],
      resourceVersions: [{ revision, entitySequenceReset: true }],
    });
    await observe(frame("initial", 7));
    await expect(observe(frame("change", 7))).resolves.toMatchObject({
      sequence: 7,
      stateDocuments: [],
    });
    expect(requests).toEqual([7, 7]);
    await expect(observe(frame("change", 6))).rejects.toThrow(/backwards/);
    await expect(
      observe(frame("change", 7, "different-workspace")),
    ).rejects.toThrow(/workspace changed/);
    reset = true;
    await expect(observe(frame("change", 7))).rejects.toThrow(/sequence reset/);
  });

  it.runIf(process.platform === "linux")(
    "observes and releases two real FIFO-blocked child processes",
    async () => {
      const directory = await mkdtemp(join(tmpdir(), "scale-fifo-"));
      temporaryPaths.push(directory);
      const runId = "11111111-1111-4111-8111-111111111111";
      const tokens = [
        "11111111-1111-4111-8111-111111111111",
        "22222222-2222-4222-8222-222222222222",
      ];
      const roots = tokens.map((_, index) =>
        join(directory, `.codex-scale-${runId}-${index}`),
      );
      await Promise.all(roots.map((root) => mkdir(root)));
      const releasePaths = roots.map((root) => join(root, "release.fifo"));
      const workers = tokens.map((token, index) => ({
        path: releasePaths[index],
        token,
      }));
      const created = spawnSync("mkfifo", releasePaths, { timeout: 5_000 });
      expect(created.status).toBe(0);
      const children = releasePaths.map((releasePath, index) =>
        spawn(
          process.execPath,
          [
            "-e",
            `const fs=require('node:fs');const fd=fs.openSync(process.argv[1],'r+');process.stdout.write('READY');const b=Buffer.alloc(64);const n=fs.readSync(fd,b,0,b.length,null);if(b.subarray(0,n).toString().trim()!==process.argv[2])process.exit(4);process.stdout.write('released');`,
            releasePath,
            tokens[index],
          ],
          { stdio: ["ignore", "pipe", "pipe"] },
        ),
      );
      const exits = children.map((child) => {
        let stderr = "";
        child.stderr.setEncoding("utf8");
        child.stderr.on("data", (chunk) => (stderr += chunk));
        return new Promise((resolve) => {
          let spawnError = null;
          child.once("error", (error) => {
            spawnError = error;
          });
          child.once("close", (code, signal) =>
            resolve({ code, signal, spawnError, stderr }),
          );
        });
      });
      const stopAt = performance.now() + 5_000;
      try {
        await Promise.all(
          children.map(
            (child) =>
              new Promise((resolve, reject) => {
                let stdout = "";
                child.stdout.setEncoding("utf8");
                const timer = setTimeout(
                  () => reject(new Error("child FIFO open timed out")),
                  Math.max(1, stopAt - performance.now()),
                );
                child.stdout.on("data", (chunk) => {
                  stdout += chunk;
                  if (stdout.includes("READY")) {
                    clearTimeout(timer);
                    resolve();
                  }
                });
                child.once("close", (code) => {
                  clearTimeout(timer);
                  if (!stdout.includes("READY"))
                    reject(new Error(`child exited before readiness: ${code}`));
                });
                child.once("error", reject);
              }),
          ),
        );
        await releaseFifoBarriers(workers, stopAt);
        const results = await Promise.all(exits);
        expect(results.map((result) => result.code)).toEqual([0, 0]);
        expect(results.map((result) => result.spawnError)).toEqual([
          null,
          null,
        ]);
      } finally {
        for (const child of children) {
          if (child.exitCode === null && child.signalCode === null)
            child.kill("SIGKILL");
        }
        await Promise.all(exits);
      }
    },
  );
});

describe("scale verifier readiness markers", () => {
  const newRoot = async () => {
    const root = await mkdtemp(join(tmpdir(), "scale-markers-"));
    temporaryPaths.push(root);
    return root;
  };
  const publish = async (directory, index, content) => {
    const temp = join(directory, `worker-${index}.ready.tmp`);
    const final = join(directory, `worker-${index}.ready`);
    await writeFile(temp, content, { mode: 0o600 });
    await rename(temp, final);
  };
  const soon = () => performance.now() + 5_000;

  it("creates one private marker directory and never reuses an existing path", async () => {
    const root = await newRoot();
    const directory = join(root, "markers");
    await createReadinessMarkerDirectory(directory);
    expect((await stat(directory)).mode & 0o777).toBe(0o700);
    await expect(createReadinessMarkerDirectory(directory)).rejects.toThrow();
  });

  it("delivers a marker published after arming and ignores temporary names", async () => {
    const root = await newRoot();
    const directory = await createReadinessMarkerDirectory(join(root, "m"));
    const tokens = [randomUUID(), randomUUID()];
    const watch = await watchReadinessMarkers(directory, tokens);
    try {
      // A partial temporary file is not a marker until its rename publishes it.
      await writeFile(join(directory, "worker-1.ready.tmp"), "partial", {
        mode: 0o600,
      });
      await expect(watch.next(performance.now() + 100)).rejects.toThrow(
        /Timed out/,
      );
      await publish(directory, 1, `${tokens[1]}\n`);
      expect(await watch.next(soon())).toBe(1);
      expect(watch.path(1)).toBe(join(directory, "worker-1.ready"));
    } finally {
      watch.close();
    }
  });

  it("accepts a marker published before arming through the initial inspection", async () => {
    const root = await newRoot();
    const directory = await createReadinessMarkerDirectory(join(root, "m"));
    const tokens = [randomUUID()];
    await publish(directory, 0, `${tokens[0]}\n`);
    const watch = await watchReadinessMarkers(directory, tokens);
    try {
      expect(await watch.next(soon())).toBe(0);
    } finally {
      watch.close();
    }
  });

  it("delivers each marker once when publication races the initial inspection", async () => {
    const root = await newRoot();
    const directory = await createReadinessMarkerDirectory(join(root, "m"));
    const tokens = [randomUUID()];
    await publish(directory, 0, `${tokens[0]}\n`);
    const watch = await watchReadinessMarkers(directory, tokens);
    try {
      expect(await watch.next(soon())).toBe(0);
      await expect(watch.next(performance.now() + 100)).rejects.toThrow(
        /Timed out/,
      );
    } finally {
      watch.close();
    }
  });

  it("rejects a marker that does not hold its exact token", async () => {
    const root = await newRoot();
    const directory = await createReadinessMarkerDirectory(join(root, "m"));
    const tokens = [randomUUID()];
    const watch = await watchReadinessMarkers(directory, tokens);
    try {
      await publish(directory, 0, `${randomUUID()}\n`);
      await expect(watch.next(soon())).rejects.toThrow(/exact token/);
    } finally {
      watch.close();
    }
  });

  it("rejects unknown marker names and symbolic-link markers", async () => {
    const root = await newRoot();
    const directory = await createReadinessMarkerDirectory(join(root, "m"));
    const tokens = [randomUUID(), randomUUID()];
    const unknown = await watchReadinessMarkers(directory, tokens);
    try {
      await publish(directory, 7, `${tokens[0]}\n`);
      await expect(unknown.next(soon())).rejects.toThrow(/Unknown readiness/);
    } finally {
      unknown.close();
    }
    const linked = await createReadinessMarkerDirectory(join(root, "linked"));
    const target = join(root, "target");
    await writeFile(target, `${tokens[0]}\n`, { mode: 0o600 });
    await symlink(target, join(linked, "worker-0.ready"));
    const watch = await watchReadinessMarkers(linked, tokens);
    try {
      await expect(watch.next(soon())).rejects.toThrow(/symbolic link/);
    } finally {
      watch.close();
    }
  });

  it("times out, closes, and aborts pending waits without leaking the watcher", async () => {
    const root = await newRoot();
    const directory = await createReadinessMarkerDirectory(join(root, "m"));
    const tokens = [randomUUID()];
    const timedOut = await watchReadinessMarkers(directory, tokens);
    await expect(timedOut.next(performance.now() + 50)).rejects.toThrow(
      /Timed out/,
    );
    timedOut.close();

    const closed = await watchReadinessMarkers(directory, tokens);
    const pendingClose = closed.next(soon());
    closed.close();
    await expect(pendingClose).rejects.toThrow(/closed/);

    const controller = new AbortController();
    const aborted = await watchReadinessMarkers(
      directory,
      tokens,
      controller.signal,
    );
    const pendingAbort = aborted.next(soon());
    controller.abort(new Error("Stopped by the operator."));
    await expect(pendingAbort).rejects.toThrow("Stopped by the operator.");
    aborted.close();
  });

  it("rejects an already aborted readiness watch before opening it", async () => {
    const controller = new AbortController();
    controller.abort(new Error("Already stopped."));
    await expect(
      watchReadinessMarkers("unused", [], controller.signal),
    ).rejects.toThrow("Already stopped.");
  });

  it("does not use a queued creation timestamp as a process retirement bound", () => {
    expect(() =>
      callbackProcessRetirementDelayMs({
        startedAt: "2026-10-18T00:00:00.000Z",
        preparedAt: "2026-10-18T00:00:00.000Z",
        nowMs: Date.parse("2026-10-18T00:10:00.000Z"),
      }),
    ).toThrow(/no verified readiness bound/);
  });

  it("bounds callback retirement from an observed marker without any transcript row", () => {
    const readinessObservedAt = "2026-10-18T00:00:00.000Z";
    const startMs = Date.parse(readinessObservedAt);
    expect(
      callbackProcessRetirementDelayMs({
        readinessObservedAt,
        nowMs: startMs + 10_000,
      }),
    ).toBe(TOOL_BARRIER_PROCESS_BOUND_MS - 10_000);
    expect(
      callbackProcessRetirementDelayMs({
        readinessObservedAt,
        nowMs: startMs + TOOL_BARRIER_PROCESS_BOUND_MS,
      }),
    ).toBe(0);
    expect(() =>
      callbackProcessRetirementDelayMs({
        readinessObservedAt: undefined,
        nowMs: 0,
      }),
    ).toThrow(/no verified readiness bound/);
  });
});

describe("scale verifier cleanup and served-model witnesses", () => {
  const profile = {
    id: "agent-1",
    model: "claude-haiku-fixture",
    effort: SCALE_PROVIDER.effort,
    fastMode: SCALE_PROVIDER.fastMode,
  };

  it("accepts only the stored profile that matches the approved request", () => {
    expect(() =>
      assertStoredScaleProfile({ ...profile }, profile),
    ).not.toThrow();
    expect(() =>
      assertStoredScaleProfile(
        { ...profile, model: "claude-opus-fixture" },
        profile,
      ),
    ).toThrow(/stored model claude-opus-fixture/);
    expect(() =>
      assertStoredScaleProfile({ ...profile, effort: "high" }, profile),
    ).toThrow(/stored effort high/);
    expect(() =>
      assertStoredScaleProfile({ ...profile, fastMode: true }, profile),
    ).toThrow(/stored fastMode true/);
    expect(() => assertStoredScaleProfile(null, profile)).toThrow(
      /no stored profile/,
    );
  });

  it("requires served Haiku usage for every completed exchange", () => {
    const haiku = {
      model: "claude-haiku-fixture",
      responseId: "r",
      turnId: "t1",
    };
    expect(() =>
      assertServedModelEvidence(
        [haiku, { ...haiku, turnId: "t2" }],
        "agent-1",
        2,
      ),
    ).not.toThrow();
    expect(() =>
      assertServedModelEvidence(
        [haiku, { ...haiku, responseId: "tool-reply" }],
        "agent-1",
        2,
      ),
    ).toThrow(/served model is unproven/);
    expect(() => assertServedModelEvidence([haiku], "agent-1", 2)).toThrow(
      /served model is unproven/,
    );
    expect(() => assertServedModelEvidence([], "agent-1", 1)).toThrow(
      /served model is unproven/,
    );
    expect(() =>
      assertServedModelEvidence(
        [
          haiku,
          { model: "claude-opus-fixture", responseId: "r2", turnId: "t2" },
        ],
        "agent-1",
        2,
      ),
    ).toThrow(/was served claude-opus-fixture/);
    expect(() =>
      assertServedModelEvidence(
        [{ model: null, responseId: "r", turnId: "t1" }],
        "agent-1",
        1,
      ),
    ).toThrow(/was served no model/);
  });

  it("proves unsent cleanup only from one cancelled initial event and its exact attempt", async () => {
    const directory = await mkdtemp(join(tmpdir(), "studio-scale-cleanup-"));
    temporaryPaths.push(directory);
    const setup = spawnSync(
      "python3",
      [
        "-c",
        String.raw`
import json,pathlib,sqlite3,sys
with sqlite3.connect(pathlib.Path(sys.argv[1])/'canvas.sqlite3') as db:
 db.executescript("CREATE TABLE sync_identity(id TEXT PRIMARY KEY); INSERT INTO sync_identity VALUES ('workspace'); CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT); CREATE TABLE runtime_events(id TEXT PRIMARY KEY,agent TEXT,status TEXT);")
 for agent in ['unsent','unknown','submitted','extra-input','delivered']:
  event=agent+':initial'
  attempt={'submitted':agent=='submitted','executionOutcome':'unknown' if agent=='unknown' else 'unsent','events':[event]}
  db.execute('INSERT INTO runtime_agents VALUES (?,?)',(agent,json.dumps({'deletedAt':1,'startAttempt':attempt})))
  db.execute('INSERT INTO runtime_events VALUES (?,?,?)',(event,agent,'delivered' if agent=='delivered' else 'cancelled'))
  if agent=='extra-input':db.execute('INSERT INTO runtime_events VALUES (?,?,?)',('later',agent,'pending'))
`,
        directory,
      ],
      { encoding: "utf8" },
    );
    expect(setup.status, setup.stderr).toBe(0);
    const rows = await readCurrentTeamChildren(
      directory,
      "workspace",
      "lead",
      performance.now() + 5000,
      ["unsent", "unknown", "submitted", "extra-input", "delivered"],
    );
    expect(
      Object.fromEntries(rows.map((row) => [row.id, row.initialInputNotSent])),
    ).toEqual({
      unsent: true,
      unknown: false,
      submitted: false,
      "extra-input": false,
      delivered: false,
    });
  });

  it("reads served models from the separate analytics database and fences workspace identity", async () => {
    const directory = await mkdtemp(join(tmpdir(), "studio-scale-usage-"));
    temporaryPaths.push(directory);
    const setup = spawnSync(
      "python3",
      [
        "-c",
        String.raw`
import json,pathlib,sqlite3,sys
root=pathlib.Path(sys.argv[1])
with sqlite3.connect(root/'canvas.sqlite3') as canvas:
 canvas.execute('CREATE TABLE sync_identity(id TEXT PRIMARY KEY)')
 canvas.execute("INSERT INTO sync_identity VALUES ('workspace')")
with sqlite3.connect(root/'analytics.sqlite3') as analytics:
 analytics.execute('CREATE TABLE analytics_usage(seq INTEGER PRIMARY KEY,agent TEXT,turn TEXT,record TEXT)')
 for agent,turn,model in [('owned','first','claude-haiku-fixture'),('owned','second','claude-haiku-fixture'),('foreign','other','claude-opus-fixture')]:
  analytics.execute('INSERT INTO analytics_usage(agent,turn,record) VALUES (?,?,?)',(agent,turn,json.dumps({'model':model,'responseId':turn,'usageSource':'claudeResponse'})))
 analytics.execute('INSERT INTO analytics_usage(agent,turn,record) VALUES (?,?,?)',('codex-owned','turn-1',json.dumps({'model':'gpt-6-luna'})))
`,
        directory,
      ],
      { encoding: "utf8" },
    );
    expect(setup.status, setup.stderr).toBe(0);
    const records = readServedModelRecords(
      directory,
      "workspace",
      ["owned"],
      performance.now() + 5000,
    );
    expect([...records.keys()]).toEqual(["owned"]);
    expect(records.get("owned").map((row) => row.turnId)).toEqual([
      "first",
      "second",
    ]);
    expect(() =>
      assertServedModelEvidence(records.get("owned"), "owned", 2),
    ).not.toThrow();
    // Codex usage rows carry no Claude response source.
    const codex = readServedModelRecords(
      directory,
      "workspace",
      ["owned", "codex-owned"],
      performance.now() + 5000,
      null,
    );
    expect([...codex.keys()]).toEqual(["codex-owned"]);
    expect(codex.get("codex-owned")[0].model).toBe("gpt-6-luna");
    expect(() =>
      readServedModelRecords(
        directory,
        "wrong-workspace",
        ["owned"],
        performance.now() + 5000,
      ),
    ).toThrow(/workspace identity changed/);
  });

  it("joins numeric and decimal-string request identities without coercing other values", () => {
    expect(requestIdentityKey(7)).toBe(requestIdentityKey("7"));
    expect(requestIdentityKey("007")).not.toBe(requestIdentityKey(7));
    expect(requestIdentityKey(0)).toBe("0");
    for (const value of [null, undefined, "", -1, 1.5, 2 ** 53, true])
      expect(requestIdentityKey(value)).toBeNull();
  });

  it("flags a live row that was never journal-owned, including one created late", () => {
    const rows = [
      { id: "owned", deletedAt: null },
      { id: "late-foreign", deletedAt: null },
      { id: "deleted-foreign", deletedAt: "2026-10-18T00:00:00Z" },
    ];
    expect(
      foreignLiveTeamRows(rows, new Set(["owned"])).map((r) => r.id),
    ).toEqual(["late-foreign"]);
    // Absent branch: no owner may remain live, so any live row is foreign.
    expect(foreignLiveTeamRows(rows, new Set()).map((r) => r.id)).toEqual([
      "owned",
      "late-foreign",
    ]);
    expect(foreignLiveTeamRows([{ id: "owned" }], new Set(["owned"]))).toEqual(
      [],
    );
  });

  it("treats cleanup as finished from current rows without waiting for an event", () => {
    const expected = ["a", "b", "c"];
    const current = [
      { id: "a", deletedAt: "2026-10-18T00:00:00Z" },
      {
        id: "b",
        deletedAt: "2026-10-18T00:00:00Z",
        workspaceCleanupIntent: {},
      },
      { id: "c", deletedAt: null },
    ];
    expect([...finishedWorkspaceCleanupIds(current, expected)]).toEqual(["a"]);
    expect([...finishedWorkspaceCleanupIds([], expected)]).toEqual([]);
    expect([
      ...finishedWorkspaceCleanupIds(
        [
          { id: "a", deletedAt: "t" },
          { id: "b", deletedAt: "t" },
          { id: "c", deletedAt: "t" },
        ],
        expected,
      ),
    ]).toEqual(expected);
  });
});

describe("scale verifier retained-input audit", () => {
  const ownedAgentIds = new Set(["lead", "worker"]);
  const parentAgentIds = new Set(["lead"]);
  const foreignHeld = {
    id: "foreign-held",
    agent: "foreign-agent",
    kind: "user",
    status: "pending",
  };
  const audit = (phase, rows, baseline = [foreignHeld]) =>
    classifyRetainedInputs({
      rows,
      baseline,
      ownedAgentIds,
      parentAgentIds,
      phase,
    });
  const stageAudit = (rows, baseline) => audit("beforeCleanup", rows, baseline);
  const finalAudit = (rows, baseline) => audit("afterCleanup", rows, baseline);
  const drainedQueues = () => ({
    recoveryPending: 0,
    durableInputPending: 7,
    "account1.callbacks": 0,
    "account1.clock_replies": 0,
    "account1.tool_requests": 0,
    "account2.callbacks": 0,
    "account2.clock_replies": 0,
    "account2.tool_requests": 0,
  });

  it("drains the non-durable queues and reports the durable total without zeroing it", () => {
    expect(classifyStageQueues(drainedQueues())).toEqual({
      state: "drained",
      nonzero: [],
      unknown: [],
      missing: [],
      durableInputPending: 7,
    });
  });

  it("keeps a stage not drained for any nonzero dispatch, callback, clock, or recovery depth", () => {
    for (const [key, value] of [
      ["recoveryPending", 1],
      ["account2.tool_requests", 2],
      ["account1.clock_replies", 1],
    ]) {
      const result = classifyStageQueues({ ...drainedQueues(), [key]: value });
      expect(result.state).toBe("not-drained");
      expect(result.nonzero).toEqual([[key, value]]);
    }
  });

  it("treats missing, null, negative, fractional, and unrecognized counters as unavailable", () => {
    const withoutClockReplies = drainedQueues();
    delete withoutClockReplies["account2.clock_replies"];
    for (const queues of [
      { ...drainedQueues(), "account1.callbacks": null },
      { ...drainedQueues(), recoveryPending: -1 },
      { ...drainedQueues(), "account2.callbacks": 1.5 },
      { ...drainedQueues(), [DURABLE_INPUT_QUEUE_KEY]: undefined },
      withoutClockReplies,
      { ...drainedQueues(), "account1.futureQueue": 0 },
      { recoveryPending: 0, [DURABLE_INPUT_QUEUE_KEY]: 0 },
      null,
    ])
      expect(classifyStageQueues(queues).state).toBe("unavailable");
  });

  it("accepts a preexisting held foreign input only when it is unchanged and owned scope is zero", () => {
    expect(stageAudit([foreignHeld])).toEqual({
      phase: "beforeCleanup",
      foreignRetained: 1,
      foreignBaseline: 1,
      ownedRetained: 0,
      allowedParentNoticeIds: [],
    });
    expect(finalAudit([foreignHeld])).toMatchObject({
      phase: "afterCleanup",
      foreignRetained: 1,
      ownedRetained: 0,
    });
  });

  it("fails an owned pending or uncertain input before cleanup", () => {
    expect(() =>
      stageAudit([
        foreignHeld,
        {
          id: "worker-exchange",
          agent: "worker",
          kind: "user",
          status: "pending",
        },
      ]),
    ).toThrow(
      /owned input\(s\) remain retained: worker-exchange \(user, pending\)/,
    );
    expect(() =>
      stageAudit([
        foreignHeld,
        {
          id: "worker-exchange",
          agent: "worker",
          kind: "user",
          status: "uncertain",
        },
      ]),
    ).toThrow(/worker-exchange \(user, uncertain\)/);
  });

  it("fails a changed foreign identity even when the aggregate count is unchanged", () => {
    expect(() =>
      stageAudit([
        {
          id: "foreign-replacement",
          agent: "foreign-agent",
          kind: "user",
          status: "pending",
        },
      ]),
    ).toThrow(
      /consumed or changed: \["foreign-held"\].*introduced: \["foreign-replacement"\]/,
    );
  });

  it("fails a foreign input whose status moved while its identity stayed", () => {
    expect(() =>
      stageAudit([{ ...foreignHeld, status: "dispatching" }]),
    ).toThrow(/contaminated/);
  });

  it("fails a foreign input that was consumed or newly retained", () => {
    expect(() => stageAudit([])).toThrow(
      /consumed or changed: \["foreign-held"\]/,
    );
    expect(() =>
      stageAudit([
        foreignHeld,
        {
          id: "foreign-late",
          agent: "foreign-agent",
          kind: "user",
          status: "reserved",
        },
      ]),
    ).toThrow(/introduced: \["foreign-late"\]/);
  });

  it("allows a paused parent's pending child-result notices before cleanup only", () => {
    const notice = {
      id: "child:worker:turn",
      agent: "lead",
      kind: PARENT_NOTICE_INPUT_KIND,
      status: "pending",
    };
    expect(stageAudit([foreignHeld, notice])).toMatchObject({
      ownedRetained: 1,
      allowedParentNoticeIds: ["child:worker:turn"],
    });
    expect(() => finalAudit([foreignHeld, notice])).toThrow(
      /afterCleanup: owned input\(s\) remain retained: child:worker:turn \(child_result, pending\)/,
    );
  });

  it("does not allow other owned inputs to pass as parent notices", () => {
    const row = (agent, kind, status) => ({
      id: `${agent}-${kind}-${status}`,
      agent,
      kind,
      status,
    });
    expect(() =>
      stageAudit([foreignHeld, row("lead", "user", "pending")]),
    ).toThrow(/remain retained/);
    expect(() =>
      stageAudit([
        foreignHeld,
        row("lead", PARENT_NOTICE_INPUT_KIND, "uncertain"),
      ]),
    ).toThrow(/remain retained/);
    expect(() =>
      stageAudit([
        foreignHeld,
        row("worker", PARENT_NOTICE_INPUT_KIND, "pending"),
      ]),
    ).toThrow(/remain retained/);
  });

  it("rejects malformed retained-input evidence and unknown phases", () => {
    expect(() =>
      stageAudit([
        {
          id: "delivered-row",
          agent: "foreign-agent",
          kind: "user",
          status: "delivered",
        },
      ]),
    ).toThrow(/malformed/);
    expect(() =>
      classifyRetainedInputs({
        rows: [],
        baseline: [],
        ownedAgentIds,
        parentAgentIds,
        phase: "later",
      }),
    ).toThrow(/Unknown retained-input phase/);
  });

  it("reads every agent's retained inputs from one fenced snapshot without message text", async () => {
    const directory = await mkdtemp(join(tmpdir(), "studio-scale-retained-"));
    temporaryPaths.push(directory);
    const setup = spawnSync(
      "python3",
      [
        "-c",
        String.raw`
import pathlib,sqlite3,sys
root=pathlib.Path(sys.argv[1])
with sqlite3.connect(root/'canvas.sqlite3') as canvas:
 canvas.execute('CREATE TABLE sync_identity(id TEXT PRIMARY KEY)')
 canvas.execute("INSERT INTO sync_identity VALUES ('workspace')")
 canvas.execute('CREATE TABLE runtime_events(id TEXT PRIMARY KEY,agent TEXT NOT NULL,kind TEXT NOT NULL,status TEXT NOT NULL,text TEXT NOT NULL)')
 rows=[('foreign-held','foreign-agent','user','pending'),('foreign-uncertain','foreign-agent','followup','uncertain'),('worker-initial','worker','user','delivered'),('child:worker:1','lead','child_result','pending'),('lead-cancelled','lead','user','cancelled')]
 for row in rows: canvas.execute('INSERT INTO runtime_events VALUES (?,?,?,?,?)',row+('secret message text',))
`,
        directory,
      ],
      { encoding: "utf8" },
    );
    expect(setup.status, setup.stderr).toBe(0);
    const rows = readRetainedInputRows(
      directory,
      "workspace",
      performance.now() + 5000,
    );
    expect(rows).toEqual([
      {
        id: "child:worker:1",
        agent: "lead",
        kind: "child_result",
        status: "pending",
      },
      {
        id: "foreign-held",
        agent: "foreign-agent",
        kind: "user",
        status: "pending",
      },
      {
        id: "foreign-uncertain",
        agent: "foreign-agent",
        kind: "followup",
        status: "uncertain",
      },
    ]);
    expect(JSON.stringify(rows)).not.toContain("secret message text");
    expect(() =>
      readRetainedInputRows(
        directory,
        "wrong-workspace",
        performance.now() + 5000,
      ),
    ).toThrow(/workspace identity/);
  });

  it("keeps the retained statuses identical to the server's durableInputPending predicate", async () => {
    const repositoryRoot = resolve(
      dirname(fileURLToPath(import.meta.url)),
      "../../../../..",
    );
    const source = await readFile(
      join(
        repositoryRoot,
        "workspaces/runtime/packages/runtime-services/src/diagnostics/store.rs",
      ),
      "utf8",
    );
    expect(source).toContain(
      `IN (${RETAINED_INPUT_STATUSES.map((status) => `'${status}'`).join(",")})`,
    );
    const route = await readFile(
      join(
        repositoryRoot,
        "workspaces/runtime/packages/runtime-http/src/routes/system_reads.rs",
      ),
      "utf8",
    );
    expect(route).toContain(`"${DURABLE_INPUT_QUEUE_KEY}"`);
  });

  it("keeps the parent notice kind and account queue names on their runtime writers", async () => {
    const repositoryRoot = resolve(
      dirname(fileURLToPath(import.meta.url)),
      "../../../../..",
    );
    const notices = await readFile(
      join(
        repositoryRoot,
        "workspaces/runtime/packages/runtime-services/src/lifecycle_effects.rs",
      ),
      "utf8",
    );
    expect(notices).toContain(`"${PARENT_NOTICE_INPUT_KIND}"`);
    const ports = await readFile(
      join(
        repositoryRoot,
        "workspaces/runtime/apps/runtime-server/src/provider_ports.rs",
      ),
      "utf8",
    );
    for (const name of ["callbacks", "clock_replies", "tool_requests"])
      expect(ports).toContain(`"{alias}.${name}"`);
    expect(ports).toContain('"recoveryPending"');
  });
});

describe("staged run controls", () => {
  it("parses a stage list and rejects unsafe counts", () => {
    expect(parseStages("30")).toEqual([30]);
    expect(parseStages("30,128,512")).toEqual([30, 128, 512]);
    for (const bad of ["", "0", "513", "1.5", "a", "30,"])
      expect(() => parseStages(bad), bad).toThrow("--stages");
  });

  it("bounds a released callback's exit from the release time", () => {
    const now = Date.parse("2026-10-18T10:00:00Z");
    expect(
      callbackProcessRetirementDelayMs({
        readinessObservedAt: "2026-10-18T09:59:00Z",
        releasedAt: "2026-10-18T10:00:00Z",
        nowMs: now + 1_000,
      }),
    ).toBe(4_000);
  });

  it("releases a live FIFO reader and reports a vanished one", async () => {
    const directory = mkdtempSync(join(tmpdir(), "scale-fifo-"));
    temporaryPaths.push(directory);
    const path = join(directory, "release.fifo");
    spawnSync("mkfifo", [path]);
    const token = randomUUID();
    expect(releaseBarriersNow([{ path, token }])[0].outcome).toBe("gone");
    const reader = spawn("sh", ["-c", `exec 3<>${path}; read r <&3; echo $r`], {
      stdio: ["ignore", "pipe", "ignore"],
    });
    let output = "";
    reader.stdout.on("data", (chunk) => (output += chunk));
    const closed = new Promise((resolve) => reader.on("close", resolve));
    for (let i = 0; i < 100 && output === ""; i += 1) {
      if (releaseBarriersNow([{ path, token }])[0].outcome === "released")
        break;
      await new Promise((resolve) => setTimeout(resolve, 20));
    }
    await closed;
    expect(output.trim()).toBe(token);
    expect(
      releaseBarriersNow([{ path: join(directory, "missing"), token }])[0]
        .outcome,
    ).toBe("gone");
  });

  it("samples provider processes below the backend from a proc table", () => {
    const root = mkdtempSync(join(tmpdir(), "scale-proc-"));
    temporaryPaths.push(root);
    const add = (pid, ppid, comm, argv, rssPages, ticks) => {
      mkdirSync(join(root, String(pid)));
      const fields = Array(13).fill("0");
      fields[0] = "S";
      fields[1] = String(ppid);
      fields[11] = String(ticks);
      writeFileSync(
        join(root, String(pid), "stat"),
        `${pid} (${comm} x) ${fields.join(" ")}`,
      );
      writeFileSync(join(root, String(pid), "statm"), `0 ${rssPages} 0`);
      writeFileSync(join(root, String(pid), "cmdline"), argv.join("\0"));
    };
    add(10, 1, "backend", ["backend"], 100, 0);
    add(11, 10, "node", ["node", "/x/bin/claude"], 10, 50);
    add(12, 11, "claude", ["claude"], 30, 150);
    add(13, 10, "codex", ["codex", "app-server"], 7, 1);
    add(14, 1, "claude", ["claude"], 999, 1);
    const sample = sampleProviderProcesses(
      readProcessTable(root),
      10,
      "claude",
    );
    expect(sample).toMatchObject({
      processes: 2,
      treeProcesses: 3,
      backendRssBytes: 100 * 4096,
      totalRssBytes: 40 * 4096,
      totalCpuMs: 2_000,
    });
    expect(sample.rssBytes.max).toBe(30 * 4096);
  });
});

describe("scale verifier shared-workspace concurrency criterion", () => {
  const newRoot = async () => {
    const root = await mkdtemp(join(tmpdir(), "scale-shared-"));
    temporaryPaths.push(root);
    return root;
  };
  const turnStarts = (count) =>
    Array.from({ length: count }, (_, index) => ({
      method: "turn/start",
      operationId: `op-${index}`,
    }));

  it("parses the worker workspace option with shared as the default", () => {
    expect(parseWorkerWorkspace()).toBe("shared");
    expect(parseWorkerWorkspace(undefined)).toBe("shared");
    expect(parseWorkerWorkspace("worktree")).toBe("worktree");
    expect(() => parseWorkerWorkspace("separate")).toThrow(
      /--worker-workspace/,
    );
  });

  it("places each shared worker FIFO under its own evidence directory", () => {
    const first = sharedBarrierPaths("/evidence", 512, 0);
    const second = sharedBarrierPaths("/evidence", 512, 511);
    expect(first.path).toBe("/evidence/fifos-512/worker-0/release.fifo");
    expect(second.path).not.toBe(first.path);
    expect(() => sharedBarrierPaths("/evidence", 2, 2)).toThrow();
    expect(() => sharedBarrierPaths("relative", 2, 0)).toThrow();
  });

  it("finds the processes holding a FIFO through a fake /proc", async () => {
    const root = await newRoot();
    const fifoA = join(root, "a.fifo");
    const fifoB = join(root, "b.fifo");
    await Promise.all([writeFile(fifoA, ""), writeFile(fifoB, "")]);
    const proc = join(root, "proc");
    const addFd = async (pid, fd, target) => {
      await mkdir(join(proc, String(pid), "fd"), { recursive: true });
      await symlink(target, join(proc, String(pid), "fd", String(fd)));
    };
    await addFd(100, 3, fifoA);
    await addFd(100, 4, "/dev/null");
    await addFd(200, 3, fifoA);
    await addFd(300, 0, "/dev/null");
    await mkdir(join(proc, "self"), { recursive: true });
    const holders = findFifoHolders([fifoA, fifoB], proc);
    expect(holders.get(fifoA).sort()).toEqual([100, 200]);
    expect(holders.get(fifoB)).toEqual([]);
  });

  it("accepts a stage with every turn started and every tool alive", () => {
    const paths = ["/f/0", "/f/1", "/f/2"];
    const holders = new Map(paths.map((path, index) => [path, [10 + index]]));
    const proof = assertConcurrentWorkers({
      workers: 3,
      journalEntries: [
        ...turnStarts(4),
        { method: "turn/steer", operationId: "x" },
      ],
      fifoPaths: paths,
      holders,
    });
    expect(proof).toMatchObject({
      workers: 3,
      physicalTurnStarts: 4,
      toolProcessesHoldingFifo: 3,
    });
  });

  it("rejects missing turn starts, duplicate operations, and dead tools", () => {
    const paths = ["/f/0", "/f/1"];
    const alive = new Map(paths.map((path) => [path, [1]]));
    expect(() =>
      assertConcurrentWorkers({
        workers: 2,
        journalEntries: turnStarts(2),
        fifoPaths: paths,
        holders: alive,
      }),
    ).toThrow(/turn\/start/);
    expect(() =>
      assertConcurrentWorkers({
        workers: 2,
        journalEntries: [...turnStarts(2), ...turnStarts(2)],
        fifoPaths: paths,
        holders: alive,
      }),
    ).toThrow(/turn\/start/);
    expect(() =>
      assertConcurrentWorkers({
        workers: 2,
        journalEntries: turnStarts(3),
        fifoPaths: paths,
        holders: new Map([
          ["/f/0", [1]],
          ["/f/1", []],
        ]),
      }),
    ).toThrow(/1 of 2 tool processes/);
  });
});
