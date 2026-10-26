// Real installed CLI/SDK; all provider requests end at this disposable HTTP
// fixture. Retry and deferred-tool formats are provider output, not new input.
import assert from "node:assert/strict";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import http from "node:http";
import { randomUUID } from "node:crypto";
import {
  query,
  createSdkMcpServer,
  tool,
  forkSession,
} from "@anthropic-ai/claude-agent-sdk";
import { readHistoryIndex, verifiedHistoryMessages } from "../history-index.ts";
import { forkAtTurn, rollbackSession } from "../controls.mjs";

const base = path.join(os.homedir(), ".cache/codex-studio/pivot1-v5-history");
await fs.mkdir(base, { recursive: true });
const root = await fs.mkdtemp(path.join(base, "cli-compat-"));
let scenario,
  requests = 0;
const captures = [];
const server = http.createServer(async (req, res) => {
  let raw = "";
  for await (const chunk of req) raw += chunk;
  if (!req.url?.includes("/messages")) {
    res.writeHead(200, { "content-type": "application/json" });
    res.end("{}");
    return;
  }
  if (req.url.includes("count_tokens")) {
    res.writeHead(200, { "content-type": "application/json" });
    res.end('{"input_tokens":10}');
    return;
  }
  const body = JSON.parse(raw);
  captures.push({ scenario, body });
  requests++;
  if (scenario === "retry" && requests === 1) {
    res.writeHead(503, {
      "content-type": "application/json",
      "retry-after": "0",
    });
    res.end(
      JSON.stringify({
        type: "error",
        error: { type: "overloaded_error", message: "isolated fixture retry" },
      }),
    );
    return;
  }
  const search = scenario === "tool-search" && requests === 1;
  res.writeHead(200, { "content-type": "text/event-stream" });
  const send = (type, value) =>
    res.write(
      `event: ${type}\ndata: ${JSON.stringify({ type, ...value })}\n\n`,
    );
  send("message_start", {
    message: {
      id: "msg_" + randomUUID(),
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
    content_block: search
      ? {
          type: "tool_use",
          id: "toolu_fixture_search",
          name: "ToolSearch",
          input: {},
        }
      : { type: "text", text: "" },
  });
  send("content_block_delta", {
    index: 0,
    delta: search
      ? {
          type: "input_json_delta",
          partial_json: JSON.stringify({ query: "select:mcp__fixture__echo" }),
        }
      : { type: "text_delta", text: "fixture completed" },
  });
  send("content_block_stop", { index: 0 });
  send("message_delta", {
    delta: {
      stop_reason: search ? "tool_use" : "end_turn",
      stop_sequence: null,
    },
    usage: { output_tokens: 2 },
  });
  send("message_stop", {});
  res.end();
});
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const address = server.address();
assert(address && typeof address === "object");
const previous = process.env.CLAUDE_CONFIG_DIR;
try {
  for (const mode of ["retry", "tool-search"]) {
    scenario = mode;
    requests = 0;
    const cwd = path.join(root, mode);
    await fs.mkdir(cwd);
    process.env.CLAUDE_CONFIG_DIR = path.join(cwd, "config");
    const session = { id: randomUUID(), cwd, started: true, turns: [] };
    const options = {
      cwd,
      pathToClaudeCodeExecutable: process.env.STUDIO_CLAUDE_BIN || "claude",
      model: "claude-sonnet-4-5",
      tools: mode === "tool-search" ? ["ToolSearch"] : [],
      settingSources: [],
      strictMcpConfig: true,
      permissionMode: "bypassPermissions",
      allowDangerouslySkipPermissions: true,
      includePartialMessages: true,
      maxTurns: 4,
      ...(mode === "tool-search"
        ? {
            mcpServers: {
              fixture: createSdkMcpServer({
                name: "fixture",
                version: "1.0.0",
                tools: [
                  tool("echo", "Echo fixture", {}, async () => ({
                    content: [{ type: "text", text: "echo" }],
                  })),
                ],
              }),
            },
          }
        : {}),
      env: {
        ...process.env,
        ANTHROPIC_API_KEY: "fixture-key",
        ANTHROPIC_AUTH_TOKEN: "",
        CLAUDE_CODE_OAUTH_TOKEN: "",
        ANTHROPIC_BASE_URL: `http://127.0.0.1:${address.port}`,
        CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC: "1",
        DISABLE_AUTOUPDATER: "1",
        CLAUDE_CODE_DISABLE_FILE_CHECKPOINTING: "1",
        ENABLE_TOOL_SEARCH: "true",
      },
    };
    for (let turn = 0; turn < 2; turn++) {
      const id = randomUUID();
      const q = query({
        prompt: (async function* () {
          yield {
            type: "user",
            uuid: id,
            session_id: session.id,
            parent_tool_use_id: null,
            message: {
              role: "user",
              content: turn
                ? "follow-up input"
                : mode === "retry"
                  ? "isolated retry fixture"
                  : "find fixture echo",
            },
          };
        })(),
        options: {
          ...options,
          ...(turn ? { resume: session.id } : { sessionId: session.id }),
        },
      });
      const frames = [];
      const deadline = setTimeout(() => q.close(), 30_000);
      try {
        for await (const frame of q) frames.push(frame);
      } finally {
        clearTimeout(deadline);
        q.close();
      }
      await fs.writeFile(
        path.join(cwd, `frames-${turn}.json`),
        JSON.stringify(frames, null, 2),
      );
      assert(
        frames.some(
          (frame) =>
            frame.type === "result" && !frame.is_error && frame.num_turns > 0,
        ),
        "CLI must finish its fixture response",
      );
      session.turns.push({
        id,
        status: "completed",
        nativeMessageId: id,
        items: [{ type: "userMessage", nativeId: id }],
      });
      const index = await readHistoryIndex(session);
      assert(index, `${mode} must retain authoritative input coverage`);
      assert.equal(index.outcome([id], [id]), "delivered");
      assert.equal(index.outcome([randomUUID()], [id]), "not_applied");
    }
    const file = path.join(
      process.env.CLAUDE_CONFIG_DIR,
      "projects",
      cwd.replace(/[^a-zA-Z0-9]/g, "-"),
      session.id + ".jsonl",
    );
    const rows = (await fs.readFile(file, "utf8"))
      .trim()
      .split("\n")
      .map(JSON.parse);
    if (mode === "retry")
      assert(
        rows.some(
          (row) =>
            row.type === "system" &&
            row.subtype === "api_error" &&
            row.source === "request_retry",
        ),
      );
    else {
      assert(
        rows.some(
          (row) =>
            row.type === "user" &&
            Array.isArray(row.message.content) &&
            row.message.content.some(
              (block) =>
                block.type === "tool_result" &&
                Array.isArray(block.content) &&
                block.content.some((inner) => inner.type === "tool_reference"),
            ),
        ),
      );
      assert(
        rows.some(
          (row) =>
            row.type === "attachment" &&
            row.attachment.type.startsWith("deferred_tools"),
        ),
      );
    }
    const deps = {
      persist: async () => {},
      forkSession,
      getSessionMessages: (id, { dir }) =>
        verifiedHistoryMessages({ id, cwd: dir }),
    };
    const branch = await forkAtTurn(
      session,
      { lastTurnId: session.turns[0].id },
      deps,
    );
    assert.equal(branch.turns.length, 1);
    await rollbackSession(
      session,
      { requestId: randomUUID(), turnId: session.turns[1].id },
      deps,
    );
    assert.equal(session.turns.length, 1);
    console.log(
      JSON.stringify({
        scenario: mode,
        requests,
        present: "applied (delivered)",
        absent: "not_applied",
        fork: "verified",
        rollback: "verified",
      }),
    );
  }
  await fs.writeFile(
    path.join(root, "provider-captures.json"),
    JSON.stringify(captures, null, 2),
  );
  console.log(`Fixture evidence: ${root}`);
} finally {
  if (previous === undefined) delete process.env.CLAUDE_CONFIG_DIR;
  else process.env.CLAUDE_CONFIG_DIR = previous;
  server.closeAllConnections();
  await new Promise((resolve) => server.close(resolve));
}
