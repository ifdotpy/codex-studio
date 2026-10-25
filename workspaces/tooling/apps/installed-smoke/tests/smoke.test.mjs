import assert from "node:assert/strict";
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
