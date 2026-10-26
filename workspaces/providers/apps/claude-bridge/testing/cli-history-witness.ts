import type {
  Options,
  SDKMessage,
  SDKUserMessage,
} from "@anthropic-ai/claude-agent-sdk";
import type { Session } from "../session-state.ts";
import type { ControlDependencies } from "../controls.ts";
import type { BridgeInput } from "../bridge-protocol.ts";
import type { SessionFixtureStreamEvent } from "../fixtures/session-sdk-types.ts";
import { unusedFileControls } from "./control-fixture.ts";
// Installed CLI + SDK witness. All provider responses come from a local HTTP
// fixture; no credentials, user settings, real sessions, or model calls are used.
import assert from "node:assert/strict";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import http from "node:http";
import { randomUUID, createHash } from "node:crypto";
import { query, getSessionMessages } from "@anthropic-ai/claude-agent-sdk";
import { inputGroups, sdkInputMessages } from "../input-messages.ts";
import { readHistoryIndex, verifiedHistoryMessages } from "../history-index.ts";
import { forkAtTurn, rollbackSession } from "../controls.ts";
import { forkSession } from "@anthropic-ai/claude-agent-sdk";
import { InputQueue } from "../features.ts";
import {
  persistNativeSession,
  guardNativeSessionInput,
  guardNativeSessionStart,
  sessionStartHook,
} from "../session-identity.ts";

const base = path.join(os.homedir(), ".cache/codex-studio/pivot1-v5-history");
await fs.mkdir(base, { recursive: true });
const root = await fs.mkdtemp(path.join(base, "cli-"));
const captures: unknown[] = [];
const server = http.createServer(async (request, response) => {
  let raw = "";
  for await (const chunk of request) raw += chunk;
  if (!request.url?.includes("/messages")) {
    response.writeHead(200, { "content-type": "application/json" });
    response.end("{}");
    return;
  }
  const body: unknown = JSON.parse(raw);
  assert(
    body &&
      typeof body === "object" &&
      "model" in body &&
      typeof body.model === "string",
  );
  captures.push(body);
  if (request.url.includes("count_tokens")) {
    response.writeHead(200, { "content-type": "application/json" });
    response.end('{"input_tokens":10}');
    return;
  }
  response.writeHead(200, { "content-type": "text/event-stream" });
  const send = <E extends SessionFixtureStreamEvent["type"]>(
    type: E,
    value: Omit<
      Extract<SessionFixtureStreamEvent, { type?: E | undefined }>,
      "type"
    >,
  ) =>
    response.write(
      `event: ${type}\ndata: ${JSON.stringify({ type, ...value })}\n\n`,
    );
  const id = "msg_" + randomUUID();
  send("message_start", {
    message: {
      id,
      type: "message",
      role: "assistant",
      model: body.model,
      content: [],
      stop_reason: null,
      stop_sequence: null,
      usage: {
        input_tokens: 10,
        output_tokens: 0,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: 0,
      },
    },
  });
  send("content_block_start", {
    index: 0,
    content_block: { type: "text", text: "" },
  });
  send("content_block_delta", {
    index: 0,
    delta: { type: "text_delta", text: "fixture reply" },
  });
  send("content_block_stop", { index: 0 });
  send("message_delta", {
    delta: { stop_reason: "end_turn", stop_sequence: null },
    usage: { output_tokens: 2 },
  });
  send("message_stop", {});
  response.end();
});
await new Promise<void>((resolve) =>
  server.listen(0, "127.0.0.1", () => resolve()),
);
const address = server.address();
assert(address && typeof address === "object");
const nativeId = (identity: string): ReturnType<typeof randomUUID> => {
  const hex = createHash("sha256").update(identity).digest("hex").slice(0, 32);
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
};
const fixtureOptions = (cwd: string): Options => ({
  cwd,
  pathToClaudeCodeExecutable: process.env.STUDIO_CLAUDE_BIN || "claude",
  model: "claude-sonnet-4-5",
  tools: [],
  settingSources: [],
  strictMcpConfig: true,
  permissionMode: "dontAsk",
  includePartialMessages: true,
  maxTurns: 1,
  extraArgs: { "replay-user-messages": null },
  env: {
    ...process.env,
    ANTHROPIC_API_KEY: "fixture-key",
    ANTHROPIC_AUTH_TOKEN: "",
    CLAUDE_CODE_OAUTH_TOKEN: "",
    ANTHROPIC_BASE_URL: `http://127.0.0.1:${address.port}`,
    CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC: "1",
    DISABLE_AUTOUPDATER: "1",
    CLAUDE_CODE_DISABLE_FILE_CHECKPOINTING: "1",
  },
});
const previousConfig = process.env.CLAUDE_CONFIG_DIR;
try {
  for (const serviceFirst of [true, false]) {
    const cwd = path.join(root, serviceFirst ? "service-first" : "human-first");
    await fs.mkdir(cwd);
    process.env.CLAUDE_CONFIG_DIR = path.join(cwd, "config");
    const session: Session = {
      id: randomUUID(),
      cwd,
      started: true,
      turns: [],
    };
    for (let turnIndex = 0; turnIndex < 2; turnIndex++) {
      const identity = randomUUID();
      const parts: BridgeInput[] = [
        {
          type: "text",
          text: `service-${turnIndex}`,
          inputOrigin: "studio_event",
        },
        { type: "text", text: `human-${turnIndex}`, inputOrigin: "human" },
      ];
      if (!serviceFirst) parts.reverse();
      const groups = await inputGroups(parts, async (rows) =>
        rows.map((row) => {
          assert(row.type === "text");
          return { type: row.type, text: row.text };
        }),
      );
      const messages = sdkInputMessages(session.id, identity, groups, nativeId);
      const frames: SDKMessage[] = [];
      const q = query({
        prompt: (async function* () {
          yield* messages;
        })(),
        options: {
          ...fixtureOptions(cwd),
          ...(turnIndex ? { resume: session.id } : { sessionId: session.id }),
        },
      });
      const guard = setTimeout(() => q.close(), 20_000);
      try {
        for await (const frame of q) frames.push(frame);
      } finally {
        clearTimeout(guard);
        q.close();
      }
      await fs.writeFile(
        path.join(cwd, `frames-${turnIndex}.json`),
        JSON.stringify(frames, null, 2),
      );
      assert(
        frames.some(
          (frame) =>
            frame.type === "result" && !frame.is_error && frame.num_turns > 0,
        ),
        "installed CLI must finish its fixture response",
      );
      session.turns.push({
        id: identity,
        nativeMessageId: messages[0]?.uuid,
        status: "completed",
        items: [
          {
            type: "userMessage",
            nativeId: messages.at(-1)?.uuid,
            nativeIds: messages.map((message) => message.uuid),
          },
        ],
      });
      const index = await readHistoryIndex(session);
      assert(index, "CLI history must be complete and recognized");
      assert.equal(
        index.outcome(
          messages.map((message) => message.uuid),
          messages.map((message) => message.uuid),
        ),
        "delivered",
      );
      assert.equal(
        index.outcome(
          [randomUUID()],
          messages.map((message) => message.uuid),
        ),
        "not_applied",
      );
    }
    const visible = await getSessionMessages(session.id, { dir: cwd });
    const deps: ControlDependencies = {
      ...unusedFileControls,
      persist: async () => {},
      forkSession,
      getSessionMessages: (id, options) =>
        verifiedHistoryMessages({
          id,
          cwd: options?.dir ?? assert.fail("Missing history directory"),
        }),
    };
    const branch = await forkAtTurn(
      session,
      { lastTurnId: session.turns[0]?.id ?? assert.fail("Missing first turn") },
      deps,
    );
    assert.equal(branch.turns[0]?.items[0]?.nativeIds?.length, 2);
    await rollbackSession(
      session,
      {
        requestId: randomUUID(),
        turnId: session.turns[1]?.id ?? assert.fail("Missing second turn"),
      },
      deps,
    );
    assert.equal(session.turns.length, 1);
    console.log(
      JSON.stringify({
        order: serviceFirst ? "service/human" : "human/service",
        visibleMessages: visible.length,
        forkParts: branch.turns[0]?.items[0]?.nativeIds?.length,
        rollbackParts: session.turns[0]?.items[0]?.nativeIds?.length,
        recovery: "delivered/absent verified",
      }),
    );
  }
  const clearCwd = path.join(root, "clear");
  await fs.mkdir(clearCwd);
  process.env.CLAUDE_CONFIG_DIR = path.join(clearCwd, "config");
  const oldId = randomUUID();
  const session: Session = {
    id: oldId,
    cwd: clearCwd,
    turns: [
      {
        id: randomUUID(),
        items: [],
        fileCheckpointMessageId: randomUUID(),
        fileCheckpointSource: { nativeId: oldId, cwd: clearCwd },
      },
    ],
  };
  const persist = () =>
    fs.writeFile(path.join(clearCwd, "session.json"), JSON.stringify(session));
  const queue = new InputQueue<SDKUserMessage>();
  const push = (text: string) => {
    const id = randomUUID();
    queue.push({
      type: "user",
      uuid: id,
      session_id: session.nativeId || session.id,
      parent_tool_use_id: null,
      message: { role: "user", content: [{ type: "text", text }] },
    });
    return id;
  };
  await guardNativeSessionStart(session, persist);
  const originalInput = push("before clear");
  const frames: SDKMessage[] = [];
  let stage = 0,
    conversationId: string | undefined,
    afterClearInput: string | undefined;
  const q = query({
    prompt: queue,
    options: {
      ...fixtureOptions(clearCwd),
      sessionId: oldId,
      hooks: {
        SessionStart: [
          { hooks: [sessionStartHook(session, () => undefined, persist)] },
        ],
      },
    },
  });
  const guard = setTimeout(() => q.close(), 20_000);
  try {
    for await (const frame of q) {
      frames.push(frame);
      if (frame.type === "system" && frame.subtype === "init") {
        await persistNativeSession(
          session,
          frame.session_id,
          "init",
          undefined,
          persist,
        );
      }
      if (frame.type === "conversation_reset")
        conversationId = frame.new_conversation_id;
      if (frame.type === "result") {
        assert(!frame.is_error, "clear fixture must not fail");
        if (stage === 0) {
          stage++;
          guardNativeSessionInput(
            session,
            [{ origin: "human", blocks: [{ type: "text", text: "/clear" }] }],
            {
              inputId: "clear-input",
              turnId: randomUUID(),
              attemptId: randomUUID(),
            },
          );
          await persist();
          push("/clear");
        } else if (stage === 1) {
          stage++;
          afterClearInput = push("after clear");
        } else {
          queue.close();
          break;
        }
      }
    }
  } finally {
    clearTimeout(guard);
    q.close();
  }
  await fs.writeFile(
    path.join(clearCwd, "frames.json"),
    JSON.stringify(frames, null, 2),
  );
  const stored: Session = JSON.parse(
    await fs.readFile(path.join(clearCwd, "session.json"), "utf8"),
  );
  assert.notEqual(stored.nativeId, oldId);
  assert.equal(typeof stored.nativeId, "string");
  assert(stored.nativeId);
  assert(
    conversationId,
    "installed CLI must emit its distinct conversation reset identity",
  );
  assert.notEqual(stored.nativeId, conversationId);
  assert.equal(stored.turns[0]?.fileCheckpointMessageId, undefined);
  const resumed: SDKMessage[] = [];
  const resumeInput = randomUUID();
  const resume = query({
    prompt: (async function* () {
      yield {
        type: "user",
        uuid: resumeInput,
        message: { role: "user", content: "resume after clear" },
        parent_tool_use_id: null,
      };
    })(),
    options: { ...fixtureOptions(clearCwd), resume: stored.nativeId },
  });
  const resumeGuard = setTimeout(() => resume.close(), 20_000);
  try {
    for await (const frame of resume) resumed.push(frame);
  } finally {
    clearTimeout(resumeGuard);
    resume.close();
  }
  assert(
    resumed.some(
      (frame) =>
        frame.type === "system" &&
        frame.subtype === "init" &&
        frame.session_id === stored.nativeId,
    ),
  );
  const clearIndex = await readHistoryIndex(stored);
  assert(clearIndex);
  assert.equal(
    clearIndex.outcome(
      [afterClearInput ?? assert.fail("Missing post-clear input")],
      [afterClearInput ?? assert.fail("Missing post-clear input")],
    ),
    "delivered",
  );
  assert.equal(
    clearIndex.outcome(
      [originalInput],
      [afterClearInput ?? assert.fail("Missing post-clear input")],
    ),
    "not_applied",
  );
  assert.equal(
    clearIndex.outcome(
      [resumeInput],
      [afterClearInput ?? assert.fail("Missing post-clear input")],
    ),
    "delivered",
  );
  console.log(
    JSON.stringify({
      clear: "persisted/resumed",
      threeDistinctIds: new Set([oldId, conversationId, stored.nativeId]).size,
      history: "current native session",
      checkpoint: "invalidated",
    }),
  );
  await fs.writeFile(
    path.join(root, "provider-captures.json"),
    JSON.stringify(captures, null, 2),
  );
  console.log(`Fixture evidence: ${root}`);
} finally {
  if (previousConfig === undefined) delete process.env.CLAUDE_CONFIG_DIR;
  else process.env.CLAUDE_CONFIG_DIR = previousConfig;
  server.closeAllConnections();
  await new Promise<void>((resolve, reject) =>
    server.close((error) => (error ? reject(error) : resolve())),
  );
}
