import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { test } from "node:test";

import {
  CheckFailed,
  RUN_MARKER,
  RUN_PREFIX,
  browserCardId,
  cheapestModel,
  elapsedSeconds,
  lowestEffort,
  messageItems,
  removeRunRoot,
  syncTranscriptIds,
} from "../smoke.mjs";

test("process ages read every ps etime form", () => {
  assert.equal(elapsedSeconds("05:03"), 303);
  assert.equal(elapsedSeconds(" 1:02:03\n"), 3723);
  assert.equal(elapsedSeconds("2-01:02:03"), 2 * 86_400 + 3723);
  assert.throws(() => elapsedSeconds("soon"), CheckFailed);
});

test("the cheapest priced visible model wins, and an alias prices by its resolved model", () => {
  const pricing = {
    catalog: {
      providers: {
        openai: {
          models: {
            big: { cost: { input: 2, output: 10 } },
            small: { cost: { input: 0.1, output: 0.5 } },
          },
        },
        anthropic: {
          models: { "claude-haiku-4-5": { cost: { input: 1, output: 5 } } },
        },
      },
    },
  };
  const codex = cheapestModel(
    [
      { model: "big", supportedReasoningEfforts: ["low"] },
      {
        model: "small",
        supportedReasoningEfforts: [{ reasoningEffort: "low" }],
      },
      { model: "hidden-free", hidden: true },
      { model: "unpriced" },
    ],
    pricing,
    "codex",
  );
  assert.equal(codex.model, "small");
  assert.equal(lowestEffort(codex.efforts), "low");
  assert.equal(
    cheapestModel(
      [{ model: "haiku", resolvedModel: "claude-haiku-4-5" }],
      pricing,
      "claude",
    ).model,
    "haiku",
  );
  assert.equal(cheapestModel([{ model: "big" }], pricing, "claude"), null);
  assert.equal(lowestEffort([]), undefined);
});

test("a message counts once by its item id, client id or input id", () => {
  const items = [
    { id: "agent:m1", role: "user" },
    { id: "agent:other", role: "user", clientMessageId: "m2" },
    { id: "agent:batch", role: "user", inputs: [{ id: "m3" }] },
    { id: "agent:m1-answer", role: "assistant" },
  ];
  assert.equal(messageItems(items, "m1").length, 1);
  assert.equal(messageItems(items, "m2").length, 1);
  assert.equal(messageItems(items, "m3").length, 1);
  assert.equal(
    messageItems([...items, { id: "agent:m1", role: "user" }], "m1").length,
    2,
  );
});

test("the client transcript order comes from a full page or a stream delta", () => {
  const page = [
    {
      id: "transcript:a",
      payload: JSON.stringify({ items: [{ id: "a:1" }, { id: "a:2" }] }),
    },
  ];
  assert.deepEqual(syncTranscriptIds(page, "a"), ["a:1", "a:2"]);
  const delta = [
    {
      id: "transcript:a",
      payload: JSON.stringify({
        delta: true,
        order: ["a:1", "a:2"],
        items: [{ id: "a:2" }],
      }),
    },
  ];
  assert.deepEqual(syncTranscriptIds(delta, "a"), ["a:1", "a:2"]);
  assert.throws(() => syncTranscriptIds([], "a"), CheckFailed);
});

test("the sign-in card id matches its runtime-services owner", () => {
  // Reference value from Python's uuid.uuid5(uuid.NAMESPACE_URL, name).
  assert.equal(
    browserCardId("bridge-fixture-1"),
    "d7a590af-a491-5325-8e15-28b6c1715dcf",
  );
  const owner = fs.readFileSync(
    new URL(
      "../../../../runtime/packages/runtime-services/src/browser_cards.rs",
      import.meta.url,
    ),
    "utf8",
  );
  const body =
    /pub fn card_id\(bridge_id: &str\) -> String \{([\s\S]*?)\.to_string\(\)/.exec(
      owner,
    )?.[1];
  assert.ok(body, "browser_cards::card_id is missing");
  assert.match(body, /Uuid::new_v5\(\s*&uuid::Uuid::NAMESPACE_URL,/);
  assert.match(body, /format!\("studio-browser-open:\{bridge_id\}"\)/);
});

test("only the exact marked run folder below the temporary root is removed", () => {
  const temporary = fs.realpathSync(
    fs.mkdtempSync(path.join(os.tmpdir(), "smoke-test-")),
  );
  try {
    const run = fs.mkdtempSync(path.join(temporary, RUN_PREFIX));
    fs.writeFileSync(
      path.join(run, RUN_MARKER),
      JSON.stringify({ runId: "run-a" }),
    );
    fs.writeFileSync(path.join(run, "work.txt"), "chat output");
    assert.throws(
      () => removeRunRoot(run, "run-b", temporary),
      /belongs to run run-a/,
    );
    assert.ok(fs.existsSync(run));

    const unmarked = fs.mkdtempSync(path.join(temporary, RUN_PREFIX));
    assert.throws(() => removeRunRoot(unmarked, "run-a", temporary));
    assert.ok(fs.existsSync(unmarked));

    const other = fs.mkdtempSync(path.join(temporary, "owner-"));
    fs.writeFileSync(
      path.join(other, RUN_MARKER),
      JSON.stringify({ runId: "run-a" }),
    );
    assert.throws(
      () => removeRunRoot(other, "run-a", temporary),
      /not a run folder/,
    );
    assert.ok(fs.existsSync(other));

    const nested = path.join(run, `${RUN_PREFIX}nested`);
    fs.mkdirSync(nested);
    fs.writeFileSync(
      path.join(nested, RUN_MARKER),
      JSON.stringify({ runId: "run-a" }),
    );
    assert.throws(
      () => removeRunRoot(nested, "run-a", temporary),
      /not a run folder/,
    );

    assert.equal(removeRunRoot(run, "run-a", temporary), run);
    assert.ok(!fs.existsSync(run));
  } finally {
    fs.rmSync(temporary, { recursive: true, force: true });
  }
});

test("Linux release layout and macOS bundle layout stay distinct", async () => {
  const { installationPaths } = await import("../smoke.mjs");
  assert.deepEqual(installationPaths("/home/test/server/current", "linux"), {
    workspace: "/home/test/server/current",
    server: "/home/test/server/current/bin/studio-runtime-server",
    renderer:
      "/home/test/server/current/workspaces/client/apps/web/dist/studio-renderer.json",
  });
  assert.equal(
    installationPaths("/Applications/Studio.app", "darwin").server,
    "/Applications/Studio.app/Contents/Resources/workspace/bin/studio-runtime-server",
  );
});

test("native truth keeps final Claude usage per message and excludes double-counted reasoning", async () => {
  const { nativeResponses, sumTokens, totalTokens } =
    await import("../usage.mjs");
  const responses = nativeResponses(
    [
      {
        type: "assistant",
        message: {
          id: "m",
          model: "haiku",
          usage: { input_tokens: 4, output_tokens: 1 },
        },
      },
      {
        type: "assistant",
        message: {
          id: "m",
          model: "haiku",
          usage: {
            input_tokens: 4,
            cache_read_input_tokens: 10,
            cache_creation_input_tokens: 3,
            output_tokens: 20,
          },
        },
      },
    ],
    "claude",
  );
  assert.equal(responses.length, 1);
  assert.equal(totalTokens(sumTokens(responses)), 37);
  const codex = nativeResponses(
    [
      {
        type: "token_usage_record",
        payload: {
          response_id: "r",
          usage: {
            input_tokens: 100,
            cached_input_tokens: 80,
            output_tokens: 30,
            reasoning_output_tokens: 20,
          },
        },
      },
    ],
    "codex",
  );
  assert.deepEqual(codex[0].tokens, {
    uncachedInput: 20,
    cacheReadInput: 80,
    cacheWriteInput: 0,
    output: 30,
    reasoningOutput: 20,
  });
  assert.equal(totalTokens(sumTokens(codex)), 130);
});

test("truth cost uses every billed category and typed recorded usage wins", async () => {
  const { priceResponses, recordedResponses } = await import("../usage.mjs");
  const tokens = {
    uncachedInput: 100,
    cacheReadInput: 200,
    cacheWriteInput: 300,
    output: 400,
    reasoningOutput: 100,
  };
  const pricing = {
    catalog: {
      providers: {
        anthropic: {
          models: {
            haiku: {
              cost: { input: 1, cache_read: 0.1, cache_write: 1.25, output: 5 },
            },
          },
        },
      },
    },
  };
  assert.equal(
    priceResponses(
      [{ model: "haiku", responseId: "r", tokens }],
      pricing,
      "claude",
    ).costUsd,
    0.002495,
  );
  assert.deepEqual(
    recordedResponses(
      {
        timeline: [
          {
            responseId: "r",
            studioUsage: { kind: "response", tokens },
            requestUsage: { inputTokens: 1 },
          },
        ],
      },
      "claude",
    )[0].tokens,
    tokens,
  );
});

test("analytics collection uses a bounded API snapshot", async () => {
  const { analyticsRoute } = await import("../smoke.mjs");
  assert.equal(
    analyticsRoute("agent-a"),
    "/api/analytics?agent=agent-a&scope=agent&limit=500",
  );
});

test("usage acceptance requires the tool result and Claude steer, not just final text", async () => {
  const { scenarioEvidence } = await import("../usage.mjs");
  const root = path.join(os.tmpdir(), "smoke-fixture");
  const items = [
    { role: "user", text: "This is turn 2 of a usage check." },
    { role: "user", text: "usage-check steer", id: "steer", at: 2 },
    { role: "assistant", text: "First part" },
    {
      role: "output",
      text: JSON.stringify({
        command: "pwd",
        cwd: root,
        exitCode: 0,
        aggregatedOutput: root,
      }),
      turnId: "long",
    },
    { role: "assistant", text: "Second part" },
    { role: "user", text: "Turn 3: reply with exactly FINAL" },
  ];
  assert.equal(scenarioEvidence(items, "claude", root).assistantResponses, 2);
  assert.throws(
    () =>
      scenarioEvidence(
        items.filter((item) => item.id !== "steer"),
        "claude",
        root,
      ),
    /steer/,
  );
  assert.throws(
    () =>
      scenarioEvidence(
        items.filter((item) => item.role !== "output"),
        "codex",
        root,
      ),
    /pwd/,
  );
});

test("base-rate price proof refuses a request crossing a published context tier", async () => {
  const { priceResponses } = await import("../usage.mjs");
  const pricing = {
    catalog: {
      providers: {
        openai: {
          models: {
            small: {
              cost: {
                input: 1,
                output: 2,
                tiers: [
                  { tier: { type: "context", size: 100 }, input: 3, output: 4 },
                ],
              },
            },
          },
        },
      },
    },
  };
  assert.throws(
    () =>
      priceResponses(
        [
          {
            model: "small",
            tokens: {
              uncachedInput: 100,
              cacheReadInput: 0,
              cacheWriteInput: 0,
              output: 1,
            },
          },
        ],
        pricing,
        "codex",
      ),
    /base-rate/,
  );
});

test("an interrupt between turns prevents later paid submissions and cleanup handles a second signal", async () => {
  const { Smoke } = await import("../smoke.mjs");
  const smoke = new Smoke({
    url: "http://127.0.0.1:1",
    app: os.tmpdir(),
    model: {},
  });
  smoke.root = path.join(os.tmpdir(), "smoke-cancellation-fixture");
  const posts = [];
  smoke.backend.post = async (...args) => {
    posts.push(args);
  };
  smoke.answered = async () => {
    smoke.cancel.abort(new Error("interrupted"));
    return [];
  };
  await assert.rejects(
    smoke.usageScenario({ id: "a" }, "codex"),
    /interrupted/,
  );
  assert.equal(posts.length, 1);
  for (const method of [
    "preflight",
    "build",
    "renderer",
    "accounts",
    "session",
    "ownerChatsUnchanged",
  ])
    smoke[method] = async () => "fixture";
  smoke.makeRoot = () => {};
  smoke.accountsByProvider = new Map([["codex", "account"]]);
  smoke.turn = async () => {
    process.emit("SIGINT");
    return { id: "a" };
  };
  let cleanup = false;
  smoke.cleanup = async () => {
    assert.ok(process.listenerCount("SIGINT") > 0);
    process.emit("SIGTERM");
    cleanup = true;
  };
  await smoke.main(false);
  assert.equal(cleanup, true);
  assert.equal(process.listenerCount("SIGINT"), 0);
  assert.equal(process.listenerCount("SIGTERM"), 0);
});

test("SSE connection establishment has a deadline", async () => {
  const { LiveChanges } = await import("../smoke.mjs");
  const original = globalThis.fetch;
  globalThis.fetch = async (_url, { signal }) =>
    new Promise((_resolve, reject) =>
      signal.addEventListener("abort", () => reject(signal.reason), {
        once: true,
      }),
    );
  try {
    const stream = new LiveChanges({ url: "http://fixture" }, "a", 4, {
      connectMs: 10,
    });
    await assert.rejects(stream.ready, /SSE connection/);
    await stream.close();
  } finally {
    globalThis.fetch = original;
  }
});

test("displayed cost must cover final usage generation and have ready finite pricing", async () => {
  const { publicationReady } = await import("../usage.mjs");
  const cost = {
    rootId: "a",
    generation: 4,
    totalUSD: 0.1,
    pricedSamples: 4,
    pricingState: "ready",
    refreshing: false,
    unknownModels: [],
  };
  const analytics = {
    timeline: [{ turnId: "final", delta: { outputTokens: 1 } }],
  };
  assert.equal(publicationReady(cost, cost, analytics, "final"), true);
  assert.equal(
    publicationReady({ ...cost, generation: 3 }, cost, analytics, "final"),
    false,
  );
  assert.equal(
    publicationReady(
      { ...cost, totalUSD: null, pricingState: "loading" },
      cost,
      analytics,
      "final",
    ),
    false,
  );
  assert.equal(
    publicationReady(
      cost,
      cost,
      { timeline: [{ turnId: "earlier" }] },
      "final",
    ),
    false,
  );
});

test("bridge provenance resolves packaged paths with spaces and requires the owning server", async () => {
  const { bridgeProcessMatches } = await import("../usage.mjs");
  const bridge =
    "/Applications/Codex Studio.app/Contents/Resources/workspace/scripts/claude_bridge/bridge.mjs";
  const root = "/Users/test/Library/Application Support/Studio";
  const processes = [
    {
      pid: 40,
      ppid: 10,
      command: `/Applications/Node App/node ${bridge} ${root}`,
    },
    { pid: 41, ppid: 11, command: `/usr/bin/node ${bridge} ${root}` },
  ];
  assert.deepEqual(
    bridgeProcessMatches(processes, [bridge], root, 10).map((row) => row.pid),
    [40],
  );
  assert.equal(
    bridgeProcessMatches(processes, [bridge], root + "-other", 10).length,
    0,
  );
});

test("steer proof rejects pending delivery and the wrong submitted identity", async () => {
  const { validateSteer } = await import("../usage.mjs");
  const item = {
    id: "a:s",
    role: "user",
    at: 2,
    inputs: [
      {
        id: "s",
        requestedDelivery: "current_turn",
        deliveryStatus: "delivered",
        materialized: true,
        pending: false,
      },
    ],
  };
  const receipt = { id: "s", status: "delivered", materialized: true };
  assert.equal(
    validateSteer(item, receipt, { id: "s", turnId: "long" }, "long", 1, 3).id,
    "s",
  );
  assert.throws(
    () =>
      validateSteer(
        item,
        { ...receipt, status: "pending" },
        { id: "s", turnId: "long" },
        "long",
        1,
        3,
      ),
    /delivery/,
  );
  assert.throws(
    () =>
      validateSteer(
        item,
        receipt,
        { id: "other", turnId: "long" },
        "long",
        1,
        3,
      ),
    /identity/,
  );
  assert.throws(
    () =>
      validateSteer(item, receipt, { id: "s", turnId: "wrong" }, "long", 1, 3),
    /turn/,
  );
});

test("atomic checkpoint failure preserves the previous complete evidence", async () => {
  const { atomicWriteJson, preserveProvenance } = await import("../usage.mjs");
  const folder = fs.mkdtempSync(path.join(os.tmpdir(), "smoke-evidence-test-"));
  try {
    const file = path.join(folder, "evidence.json");
    atomicWriteJson(file, { original: true });
    assert.throws(
      () =>
        atomicWriteJson(
          file,
          { replaced: true },
          {
            ...fs,
            renameSync() {
              throw new Error("rename fixture");
            },
          },
        ),
      /rename fixture/,
    );
    assert.deepEqual(JSON.parse(fs.readFileSync(file)), { original: true });
    assert.deepEqual(fs.readdirSync(folder), ["evidence.json"]);
    const evidence = { server: { binary: "original" } };
    preserveProvenance(evidence, { binary: "observed" }, true);
    assert.equal(evidence.server.binary, "original");
    assert.equal(evidence.recoveryObservations[0].server.binary, "observed");
  } finally {
    fs.rmSync(folder, { recursive: true, force: true });
  }
});

test("cleanup recovers project registration by the exact owned path after a lost create response", async () => {
  const { Smoke } = await import("../smoke.mjs");
  const smoke = new Smoke({
    url: "http://127.0.0.1:1",
    app: os.tmpdir(),
    model: {},
  });
  smoke.makeRoot();
  const root = smoke.root;
  const posts = [];
  smoke.backend.get = async (route) =>
    route === "/api/projects"
      ? { items: [{ path: root + "-owner" }, { locations: [{ path: root }] }] }
      : { chats: [] };
  smoke.backend.post = async (...args) => {
    posts.push(args);
    return {};
  };
  try {
    await smoke.cleanup();
    assert.equal(posts.length, 1);
    assert.equal(posts[0][0], "/api/projects");
    assert.equal(posts[0][1].path, root);
    assert.equal(posts[0][1].action, "remove");
    assert.equal(fs.existsSync(root), false);
  } finally {
    if (fs.existsSync(root)) fs.rmSync(root, { recursive: true, force: true });
  }
});

test("collector waits for a matching final-turn cost publication through SSE", async () => {
  const { Smoke } = await import("../smoke.mjs");
  const smoke = new Smoke({
    url: "http://fixture",
    app: os.tmpdir(),
    model: {},
  });
  const cost = {
    rootId: "a",
    generation: 4,
    totalUSD: 0.1,
    pricedSamples: 4,
    pricingState: "ready",
    refreshing: false,
    unknownModels: [],
  };
  const analytics = {
    agents: [{ id: "a" }],
    timeline: [
      { turnId: "final", responseId: "r", delta: { outputTokens: 1 } },
    ],
  };
  let controller;
  let updated = false;
  const encoder = new TextEncoder();
  const frame = (value) =>
    encoder.encode(
      `event: resources\ndata: ${JSON.stringify({
        resourceVersions: [
          {
            entityChanges: {
              documents: [
                {
                  collection: "agentDetail",
                  id: "a",
                  value: { id: "a", tokensUsed: 100 },
                },
                { collection: "sessionCost", id: "a", value },
              ],
            },
          },
        ],
      })}\n\n`,
    );
  const original = globalThis.fetch;
  globalThis.fetch = async (_url, { signal }) =>
    new Response(
      new ReadableStream({
        start(stream) {
          controller = stream;
          signal.addEventListener(
            "abort",
            () => {
              stream.close();
              controller = null;
            },
            { once: true },
          );
          stream.enqueue(frame({ ...cost, generation: 3 }));
        },
      }),
    );
  smoke.backend.get = async (route) => {
    if (route === "/api/sync/protocol") return { protocolVersion: 4 };
    if (route.startsWith("/api/session-cost")) {
      if (!updated) {
        updated = true;
        controller.enqueue(frame(cost));
      }
      return cost;
    }
    return analytics;
  };
  try {
    const result = await smoke.ownEntities("a", "final");
    assert.equal(updated, true);
    assert.equal(result.sessionCost.generation, 4);
    assert.equal(result.publication.finalTurnId, "final");
  } finally {
    controller?.close();
    globalThis.fetch = original;
  }
});

test("supervisor account identity admits a retained bridge across backend replacement", async () => {
  const { bridgeProcessMatches } = await import("../usage.mjs");
  const bridge = "/release/workspaces/providers/apps/claude-bridge/bridge.mjs";
  const processes = [
    {
      pid: 40,
      ppid: 5,
      startTime: "identity",
      command: `/usr/bin/node ${bridge} /state/account-servers/c`,
    },
  ];
  const supervisor = {
    enabled: true,
    accountKey: "c",
    stateDir: "/state",
    health: {
      stateDir: "/state",
      handles: [
        {
          id: "account:c",
          pid: 40,
          signature: "launch",
          startTime: "identity",
          stdoutReaderAlive: true,
        },
      ],
    },
  };
  assert.equal(
    bridgeProcessMatches(
      processes,
      [bridge],
      "/state/account-servers/c",
      10,
      supervisor,
    ).length,
    1,
  );
  assert.equal(
    bridgeProcessMatches(
      processes,
      [bridge],
      "/state/account-servers/c",
      11,
      supervisor,
    ).length,
    1,
  );
  assert.equal(
    bridgeProcessMatches(processes, [bridge], "/state/account-servers/c", 11, {
      ...supervisor,
      accountKey: "other",
    }).length,
    0,
  );
});

test("recovery uses saved serving provenance when no live bridge remains", async () => {
  const { Smoke } = await import("../smoke.mjs");
  const folder = fs.mkdtempSync(path.join(os.tmpdir(), "smoke-recover-test-"));
  const file = path.join(folder, "run.json");
  const server = { sha256: "build", stateDir: "/state" };
  const savedBridge = [
    { pid: 40, bridge: "/saved/bridge.mjs", sha256: "bridge-build" },
  ];
  fs.writeFileSync(
    file,
    JSON.stringify({
      format: "studio-live-usage-v1",
      runId: "abcdef12",
      server,
      chatIds: ["a"],
      runFolder: "/owned/run",
      checks: [],
      providers: [
        { agentId: "a", provider: "claude", bridgeProcesses: savedBridge },
      ],
    }),
  );
  const smoke = new Smoke({
    url: "http://fixture",
    app: os.tmpdir(),
    model: {},
  });
  smoke.backend.get = async (route) =>
    route === "/api/desktop"
      ? { backendBuild: "build", stateDir: "/state", pid: 10 }
      : { accounts: [] };
  smoke.ownEntities = async () => ({
    agent: {
      name: "Studio smoke abcdef12 claude",
      id: "a",
      provider: "claude",
      accountKey: "c",
      model: "haiku",
    },
  });
  smoke.bridgeObservation = async () => {
    throw new Error("No live bridge");
  };
  let recovered;
  smoke.collectUsage = async (_chat, _provider, scenario) => {
    recovered = await smoke.currentBridgeProvenance("c", scenario.savedBridge);
  };
  smoke.serverObservation = () => ({ ...server, pid: 10 });
  smoke.saveUsage = () => {};
  smoke.printUsage = () => {};
  try {
    await smoke.recover(file);
    assert.deepEqual(recovered.serving, savedBridge);
    assert.equal(recovered.current.available, false);
    assert.deepEqual(smoke.usageEvidence.server, server);
  } finally {
    fs.rmSync(folder, { recursive: true, force: true });
  }
});

test("cancellation during Claude steering prevents steer and final message submissions", async () => {
  const { Smoke } = await import("../smoke.mjs");
  const smoke = new Smoke({
    url: "http://fixture",
    app: os.tmpdir(),
    model: {},
  });
  smoke.root = path.join(os.tmpdir(), "smoke-steer-fixture");
  const posts = [];
  smoke.backend.post = async (...args) => {
    posts.push(args);
  };
  smoke.transcript = async () => [{ role: "assistant", turnId: "initial" }];
  smoke.live = {
    entities: new Map([["agentSummary:a", { turnId: "long" }]]),
    changed: async () => {},
  };
  smoke.chatStatus = async () => {
    smoke.cancel.abort(new Error("steer interrupted"));
    return { inFlight: true };
  };
  smoke.saveUsage = () => {};
  await assert.rejects(
    smoke.usageScenario({ id: "a" }, "claude"),
    /steer interrupted/,
  );
  assert.equal(posts.length, 1);
});

test("usage chat stays idle until returned permissions have been verified", async () => {
  const { Smoke } = await import("../smoke.mjs");
  const original = globalThis.fetch;
  try {
    for (const unsafe of [true, false]) {
      const smoke = new Smoke({
        url: "http://fixture",
        app: os.tmpdir(),
        model: {},
        usage: true,
      });
      smoke.makeRoot();
      let permissionRecord;
      smoke.permissionRecord = () => permissionRecord;
      smoke.accountsByProvider = new Map([["codex", "default"]]);
      smoke.chooseModel = async () => ({ model: "small", efforts: [] });
      smoke.saveUsage = () => {};
      smoke.answered = async () => [];
      smoke.backend.get = async () => ({ protocolVersion: 4 });
      let controller;
      globalThis.fetch = async (_url, { signal }) =>
        new Response(
          new ReadableStream({
            start(stream) {
              controller = stream;
              signal.addEventListener("abort", () => stream.close(), {
                once: true,
              });
            },
          }),
        );
      const posts = [];
      smoke.backend.post = async (route, body) => {
        posts.push({ route, body });
        if (route === "/api/leads") {
          const record = {
            id: body.id,
            role: "orchestrator",
            yoloMode: unsafe,
            cwd: smoke.root,
            provider: "codex",
          };
          permissionRecord = record;
          controller.enqueue(
            new TextEncoder().encode(
              `event: resources\ndata: ${JSON.stringify({ resourceVersions: [{ entityChanges: { documents: [{ collection: "agentDetail", id: body.id, value: record }] } }] })}\n\n`,
            ),
          );
          return record;
        }
        if (route === "/api/rename") return {};
        assert.ok(smoke.usageEvidence.permissions[body.room]);
        return {};
      };
      try {
        if (unsafe)
          await assert.rejects(smoke.turn("codex"), /permission|YOLO/i);
        else await smoke.turn("codex");
        assert.equal(posts[0].route, "/api/leads");
        assert.equal(posts[0].body.yolo_mode, false);
        assert.equal(
          posts.filter((post) => post.route === "/api/messages").length,
          unsafe ? 0 : 1,
        );
      } finally {
        await smoke.live?.close();
        removeRunRoot(smoke.root, smoke.runId, smoke.temporaryRoot);
      }
    }
  } finally {
    globalThis.fetch = original;
  }
});

test("permission evidence rejects outside writable roots and accepts the read-only note result", async () => {
  const {
    verifiedPermissions,
    directoryIdentity,
    scenarioEvidence,
    NOTES_FILE,
    NOTES_MARKER,
  } = await import("../usage.mjs");
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "owned-note-fixture-"));
  try {
    const identity = directoryIdentity(root);
    const record = { yoloMode: false, cwd: root, role: "orchestrator" };
    assert.equal(
      verifiedPermissions(record, "claude", root, identity).permissionMode,
      "default",
    );
    assert.throws(
      () =>
        verifiedPermissions(
          {
            ...record,
            sandboxPolicy: {
              type: "workspaceWrite",
              writableRoots: [root, os.homedir()],
            },
          },
          "codex",
          root,
          identity,
        ),
      /outside/,
    );
    const transcript = [
      {
        role: "user",
        text: `This is turn 2 of a usage check. Read ${NOTES_FILE}.`,
      },
      { role: "assistant", text: "Before the read" },
      {
        role: "output",
        toolStatus: "completed",
        turnId: "long",
        text: JSON.stringify({
          type: "commandExecution",
          command: `cat ${path.join(root, NOTES_FILE)}`,
          cwd: root,
          exitCode: 0,
          aggregatedOutput: NOTES_MARKER,
        }),
      },
      { role: "assistant", text: "After the read" },
      { role: "user", text: "Turn 3: reply with exactly FINAL" },
    ];
    assert.equal(
      scenarioEvidence(transcript, "codex", root).assistantResponses,
      2,
    );
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("permission verification rejects object bypass policies, unknown policies and directory substitution", async () => {
  const { verifiedPermissions, directoryIdentity } =
    await import("../usage.mjs");
  const parent = fs.mkdtempSync(
    path.join(os.tmpdir(), "smoke-permission-test-"),
  );
  const root = path.join(parent, "run");
  fs.mkdirSync(root);
  const record = { yoloMode: false, cwd: root, role: "orchestrator" };
  try {
    const identity = directoryIdentity(root);
    assert.throws(
      () =>
        verifiedPermissions(
          { ...record, sandbox: { type: "dangerFullAccess" } },
          "codex",
          root,
          identity,
        ),
      /policy|permission/i,
    );
    assert.throws(
      () =>
        verifiedPermissions(
          {
            ...record,
            sandbox: { type: "workspaceWrite", writableRoots: [parent] },
          },
          "codex",
          root,
          identity,
        ),
      /outside/,
    );
    assert.throws(
      () =>
        verifiedPermissions(
          { ...record, sandbox: { type: "unknown" } },
          "codex",
          root,
          identity,
        ),
      /policy/,
    );
    fs.renameSync(root, path.join(parent, "previous"));
    fs.symlinkSync(path.join(parent, "previous"), root);
    assert.throws(
      () => verifiedPermissions(record, "codex", root, identity),
      /identity/,
    );
  } finally {
    fs.rmSync(parent, { recursive: true, force: true });
  }
});

test("supervisor ownership rejects unregistered backend descendants and reused PIDs", async () => {
  const { bridgeProcessMatches } = await import("../usage.mjs");
  const bridge = "/release/workspaces/providers/apps/claude-bridge/bridge.mjs";
  const row = {
    pid: 40,
    ppid: 10,
    startTime: "new-start",
    command: `/usr/bin/node ${bridge} /state/account-servers/c`,
  };
  const supervisor = {
    enabled: true,
    accountKey: "c",
    stateDir: "/state",
    health: { stateDir: "/state", handles: [] },
  };
  assert.equal(
    bridgeProcessMatches(
      [row],
      [bridge],
      "/state/account-servers/c",
      10,
      supervisor,
    ).length,
    0,
  );
  supervisor.health.handles = [
    {
      id: "account:c",
      pid: 40,
      signature: "launch",
      startTime: "old-start",
      stdoutReaderAlive: true,
    },
  ];
  assert.equal(
    bridgeProcessMatches(
      [row],
      [bridge],
      "/state/account-servers/c",
      10,
      supervisor,
    ).length,
    0,
  );
});

test("note proof refuses combined shell operations and wrong file arguments", async () => {
  const { scenarioEvidence, NOTES_FILE, NOTES_MARKER } =
    await import("../usage.mjs");
  const root = path.join(os.tmpdir(), "note-proof-fixture");
  const make = (tool) => [
    {
      role: "user",
      text: `This is turn 2 of a usage check. Read ${NOTES_FILE}.`,
    },
    { role: "assistant", text: "Before" },
    {
      role: "output",
      toolStatus: "completed",
      turnId: "long",
      text: JSON.stringify(tool),
    },
    { role: "assistant", text: "After" },
    { role: "user", text: "Turn 3: reply with exactly FINAL" },
  ];
  assert.throws(
    () =>
      scenarioEvidence(
        make({
          type: "commandExecution",
          command: `/owned/custom/sh -c cat ${root}/${NOTES_FILE}`,
          cwd: root,
          exitCode: 0,
          aggregatedOutput: NOTES_MARKER,
        }),
        "codex",
        root,
      ),
    /note read/,
  );
  assert.throws(
    () =>
      scenarioEvidence(
        make({
          type: "commandExecution",
          command: `touch changed.txt; cat ${root}/${NOTES_FILE}`,
          cwd: root,
          exitCode: 0,
          aggregatedOutput: NOTES_MARKER,
        }),
        "codex",
        root,
      ),
    /note read/,
  );
  assert.throws(
    () =>
      scenarioEvidence(
        make({
          tool: "Read",
          arguments: { file_path: root + "/other.txt" },
          result: NOTES_MARKER,
          description: NOTES_FILE,
        }),
        "codex",
        root,
      ),
    /note read/,
  );
});

test("sqlite-ro recovery reads only the exact owned Claude run", async () => {
  const { sqliteEvidence } = await import("../usage.mjs");
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "smoke-sqlite-test-"));
  const create = `import sqlite3,json,sys,pathlib
root=pathlib.Path(sys.argv[1]); c=sqlite3.connect(root/'canvas.sqlite3')
c.execute('CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT)')
for id,name in [('owned','Studio smoke abcdef12 claude'),('owner','Owner chat')]:
 c.execute('INSERT INTO runtime_agents VALUES(?,?)',(id,json.dumps({'id':id,'rootId':id,'name':name,'provider':'claude','yoloMode':False,'role':'orchestrator','cwd':str(root),'sandbox':{'type':'workspaceWrite','writableRoots':[str(root)]}})))
c.commit();c.close()
c=sqlite3.connect(root/'analytics.sqlite3')
c.execute('CREATE TABLE analytics_usage(agent TEXT,at REAL,seq INTEGER,record TEXT)')
c.execute('CREATE TABLE analytics_history(agent TEXT,record TEXT)')
c.execute('CREATE TABLE analytics_usage_roots(root TEXT,generation INTEGER)')
for id in ['owned','owner']: c.execute('INSERT INTO analytics_usage VALUES(?,?,?,?)',(id,1,1,json.dumps({'responseId':id+'-response','agentId':id})))
c.execute('INSERT INTO analytics_usage_roots VALUES(?,?)',('owned',1));c.commit()`;
  try {
    execFileSync("python3", ["-c", create, root]);
    const evidence = sqliteEvidence(root, "owned", "abcdef12", true);
    assert.equal(evidence.analytics.source, "sqlite-ro");
    assert.deepEqual(
      evidence.analytics.timeline.map((row) => row.responseId),
      ["owned-response"],
    );
    assert.equal(evidence.permissions.sandbox.type, "workspaceWrite");
    assert.throws(
      () => sqliteEvidence(root, "owner", "abcdef12", true),
      /outside the evidence run/,
    );
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("note proof rejects shell flags, metacharacters and split arguments", async () => {
  const { scenarioEvidence, NOTES_FILE, NOTES_MARKER } =
    await import("../usage.mjs");
  const root = path.join(os.tmpdir(), "shell-note-fixture");
  const file = path.join(root, NOTES_FILE);
  const make = (command) => [
    {
      role: "user",
      text: `This is turn 2 of a usage check. Read ${NOTES_FILE}.`,
    },
    { role: "assistant", text: "Before" },
    {
      role: "output",
      toolStatus: "completed",
      turnId: "long",
      text: JSON.stringify({
        type: "commandExecution",
        command,
        cwd: root,
        exitCode: 0,
        aggregatedOutput: NOTES_MARKER,
      }),
    },
    { role: "assistant", text: "After" },
    { role: "user", text: "Turn 3: reply with exactly FINAL" },
  ];
  for (const command of [
    `/bin/bash -l cat ${file}`,
    `/bin/bash -lc cat ${file}\n`,
    ...[
      "; true",
      " & true",
      " | true",
      " $HOME",
      " `true`",
      " > changed",
      " < other",
    ].map((suffix) => `/bin/bash -lc cat ${file}${suffix}`),
    `/bin/echo; /bin/bash -lc cat ${file}`,
    `/bin/bash -lc 'cat' ${file}`,
    `cat "${file}" extra`,
  ])
    assert.throws(
      () => scenarioEvidence(make(command), "codex", root),
      /note read/,
      command,
    );
});

test("failed analytics collection persists its latency before a provider row exists", async () => {
  const { Smoke, analyticsRoute } = await import("../smoke.mjs");
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "smoke-timeout-test-"));
  const file = path.join(root, "evidence.json");
  const smoke = new Smoke({
    url: "http://fixture",
    app: os.tmpdir(),
    model: {},
    usage: true,
  });
  smoke.evidencePath = file;
  smoke.saveUsage = () =>
    fs.writeFileSync(file, JSON.stringify(smoke.usageEvidence));
  smoke.backend.request = async () => {
    throw new Error("TimeoutError: request timed out");
  };
  try {
    await assert.rejects(smoke.analyticsSnapshot("a", 17), /timed out/);
    const evidence = JSON.parse(fs.readFileSync(file));
    assert.equal(evidence.providers.length, 0);
    assert.equal(evidence.analyticsLatency[0].route, analyticsRoute("a"));
    assert.equal(evidence.analyticsLatency[0].outcome, "timeout");
    assert.equal(evidence.analyticsLatency[0].timeoutMs, 17);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("recorded usage separates gaps and cumulative rows from responses", async () => {
  const { recordedUsage, recordedResponses, sumTokens } =
    await import("../usage.mjs");
  const tokens = {
    uncachedInput: 10,
    cacheReadInput: 2,
    cacheWriteInput: 0,
    output: 3,
  };
  const response = {
    kind: "response",
    responseId: "typed",
    model: "haiku",
    tokens,
  };
  const gap = { kind: "gap", responseId: "gap", reason: "queryClosed" };
  const analytics = {
    timeline: [
      { responseId: "stale", studioUsage: response },
      { responseId: "gap", studioUsage: gap, last: {} },
      {
        responseId: "associated",
        studioUsage: {
          kind: "cumulative",
          scope: "claudeQuery",
          total: tokens,
        },
        last: {},
      },
      { responseId: "notice", counterDomain: "nativeNotice", last: {} },
    ],
  };
  assert.equal(recordedResponses(analytics, "claude").length, 1);
  const recorded = recordedUsage(analytics, "claude");
  assert.equal(recorded.responses[0].responseId, "typed");
  assert.deepEqual(recorded.gaps, [
    { ...gap, tokens: null, costUsd: null, record: analytics.timeline[1] },
  ]);
  assert.equal(sumTokens(recorded.responses).output, 3);
});

test("legacy recorded categories stay unknown and use persisted adapter precedence", async () => {
  const { recordedResponses, sumTokens, totalTokens } =
    await import("../usage.mjs");
  const complete = {
    inputTokens: 10,
    cachedInputTokens: 2,
    cacheWriteInputTokens: 1,
    outputTokens: 3,
  };
  for (const source of ["delta", "requestUsage", "last"]) {
    for (const missing of [undefined, null]) {
      const responses = recordedResponses(
        {
          timeline: [
            {
              responseId: "old",
              last: complete,
              [source]: { ...complete, cachedInputTokens: missing },
            },
          ],
        },
        "codex",
      );
      assert.equal(responses[0].tokens.cacheReadInput, null);
      assert.equal(responses[0].tokens.uncachedInput, null);
      assert.equal(sumTokens(responses).cacheReadInput, null);
      assert.equal(totalTokens(sumTokens(responses)), null);
    }
  }
  const [response] = recordedResponses(
    {
      timeline: [
        {
          responseId: "old",
          usageSource: "claudeResponse",
          last: complete,
          requestUsage: { ...complete, inputTokens: 50 },
          delta: complete,
        },
      ],
    },
    "claude",
  );
  assert.equal(response.tokens.uncachedInput, 10);
});

test("comparison reprojects old snapshots and preserves unknown categories and gaps", async () => {
  const { compareEvidence } = await import("../usage.mjs");
  const old = {
    format: "old-fixture",
    providers: [
      {
        provider: "claude",
        agentId: "a",
        truth: {
          tokens: {
            uncachedInput: 10,
            cacheReadInput: 2,
            cacheWriteInput: 1,
            output: 4,
            reasoningOutput: 0,
          },
        },
        recorded: {
          tokens: {
            uncachedInput: 10,
            cacheReadInput: 0,
            cacheWriteInput: 0,
            output: 2,
            reasoningOutput: 0,
          },
        },
        budget: { tokensUsed: 12 },
        analytics: {
          timeline: [
            {
              responseId: "r",
              usageSource: "claudeResponse",
              delta: {
                inputTokens: 10,
                cachedInputTokens: null,
                cacheWriteInputTokens: 0,
                outputTokens: 2,
                reasoningOutputTokens: null,
              },
            },
            {
              responseId: "g",
              studioUsage: {
                kind: "gap",
                responseId: "g",
                reason: "queryClosed",
              },
            },
          ],
        },
      },
    ],
  };
  const original = structuredClone(old);
  const compared = compareEvidence(old, old);
  for (const row of compared) {
    assert.equal(
      row["recorded input/read/write/output/reasoning"],
      "10/unknown/0/2/unknown",
    );
    assert.equal(row["recorded/truth"], null);
    assert.equal(row["unpriced gaps"], 1);
    assert.match(row["recorded projection"], /persisted-usage-v3/);
  }
  assert.deepEqual(old, original);
  const withoutRaw = structuredClone(old);
  delete withoutRaw.providers[0].analytics;
  const [unavailable] = compareEvidence(withoutRaw, old);
  assert.equal(
    unavailable["recorded input/read/write/output/reasoning"],
    "unknown/unknown/unknown/unknown/unknown",
  );
  assert.equal(unavailable["recorded/truth"], null);
  assert.equal(unavailable["unpriced gaps"], null);
  assert.match(unavailable["recorded projection"], /unknown.*old-fixture/);
});
