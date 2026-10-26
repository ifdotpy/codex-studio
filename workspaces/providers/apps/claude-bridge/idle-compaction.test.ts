import { test, expect } from "vitest";
import {
  cacheReport,
  promptPrefix,
  idleRefusal,
  compactionEnabled,
} from "./idle-compaction.ts";

test("reports the last request TTL and context, not cumulative result usage", () => {
  const s = { cwd: "/repo", model: "opus", developerInstructions: "rules" };
  const report = cacheReport(
    s,
    {
      input_tokens: 20,
      cache_read_input_tokens: 200000,
      cache_creation_input_tokens: 5000,
      output_tokens: 30,
      cache_creation: {
        ephemeral_1h_input_tokens: 5000,
        ephemeral_5m_input_tokens: 0,
      },
    },
    "response",
    1000,
  );
  expect(report).toMatchObject({
    ttl: "1h",
    contextTokens: 205050,
    lastActivityAt: 1000,
    requestId: "response",
    warm: true,
  });
  s.idleCache = report;
  expect(
    cacheReport(
      s,
      {
        input_tokens: 20,
        cache_read_input_tokens: 205000,
        cache_creation_input_tokens: 0,
        output_tokens: 5,
      },
      "next",
      2000,
    ).ttl,
  ).toBe("1h");
  expect(
    cacheReport(
      s,
      {
        input_tokens: 20,
        cache_creation_input_tokens: 50,
        cache_creation: { ephemeral_5m_input_tokens: 50 },
      },
      "five",
      3000,
    ).ttl,
  ).toBe("5m");
  expect(
    cacheReport(
      s,
      {
        cache_creation_input_tokens: 100,
        cache_creation: {
          ephemeral_5m_input_tokens: 50,
          ephemeral_1h_input_tokens: 50,
        },
      },
      "mixed",
      3000,
    ).ttl,
  ).toBe("5m");
  expect(cacheReport({}, { input_tokens: 200000 }, "uncached", 3000).warm).toBe(
    false,
  );
  expect(
    cacheReport({}, { cache_read_input_tokens: 200000 }, "unknown", 3000).ttl,
  ).toBe("unknown");
});

test("refuses a changed prefix, newer request, active query, and disabled auto compaction", () => {
  const s = { model: "opus" };
  s.idleCache = cacheReport(
    s,
    {
      cache_creation_input_tokens: 200000,
      cache_creation: { ephemeral_1h_input_tokens: 200000 },
    },
    "a",
    1000,
  );
  const expected = { requestId: "a", prefix: promptPrefix(s) };
  expect(idleRefusal(s, null, expected)).toBe(null);
  expect(idleRefusal({ ...s, model: "sonnet" }, null, expected)).toBe(
    "prefix_changed",
  );
  expect(idleRefusal(s, { turn: {} }, expected)).toBe("active");
  expect(idleRefusal(s, { tasks: new Set(["task"]) }, expected)).toBe("active");
  expect(idleRefusal(s, null, { ...expected, requestId: "old" })).toBe(
    "newer_request",
  );
});

test("the real bridge emits request-local cache telemetry and keeps it across a bridge restart", async () => {
  const { startBridge } = await import("./testing/bridge-harness.ts");
  const sdk = String.raw`
export const resolveSettings=async()=>({effective:{}});
export const tool=(name,description,schema,call)=>({name,description,schema,call});
export const createSdkMcpServer=value=>({...value});
export const forkSession=async()=>({sessionId:'22222222-2222-4222-8222-222222222222'});
export const getSessionMessages=async()=>[];
export function query({prompt,options}) {
 const abort=new AbortController();
 return { supportedModels:async()=>[{value:'default',displayName:'Default',resolvedModel:'claude-opus-5-5'}],
 accountInfo:async()=>({email:'test@example.test',subscriptionType:'Claude Max',apiProvider:'firstParty'}),
 initializationResult:async()=>({commands:[]}),mcpServerStatus:async()=>[],reconnectMcpServer:async()=>{},
 setModel:async()=>{},setPermissionMode:async()=>{},applyFlagSettings:async()=>{},stopTask:async()=>{},setMcpServers:async()=>({added:[],removed:[],errors:{}}),
 close(){abort.abort();},interrupt:async()=>abort.abort(),
 async *[Symbol.asyncIterator]() {
  for await (const input of prompt) {
   if(abort.signal.aborted)return;
   yield {type:'user',isReplay:true,uuid:input.uuid};
   yield {type:'system',subtype:'init'};
   if(input.message.content[0].text==='/compact')yield {type:'system',subtype:'compact_boundary',uuid:'boundary',compact_metadata:{trigger:'manual',pre_tokens:210000,post_tokens:1000}};
   yield {type:'assistant',uuid:'message',message:{id:'response',role:'assistant',model:'claude-opus-5-5',content:[{type:'text',text:'done'}],usage:{input_tokens:10,cache_read_input_tokens:200000,cache_creation_input_tokens:10000,output_tokens:20,cache_creation:{ephemeral_1h_input_tokens:10000,ephemeral_5m_input_tokens:0}}}};
   yield {type:'result',subtype:'success',usage:{input_tokens:9999999,output_tokens:9999999},result:'done'};
  }
 }};
}
`;
  const bridge = await startBridge(sdk, "claude-idle-compaction-");
  try {
    const turn = await bridge.call("turn/start", {
      threadId: bridge.thread,
      clientUserMessageId: "input",
      input: [{ type: "text", text: "hello" }],
    });
    const notification = await bridge.note(
      (note) => note.method === "thread/idleCompaction/updated",
    );
    const report = notification.params.report;
    expect(report).toMatchObject({
      ttl: "1h",
      contextTokens: 210030,
      requestId: "response",
      warm: true,
    });
    await bridge.note(
      (note) =>
        note.method === "turn/completed" &&
        note.params.turn.id === turn.turn.id,
    );
    await bridge.restart();
    await expect(
      bridge.call("thread/compact/start", {
        threadId: bridge.thread,
        requestId: "idle",
        idleCompaction: { ...report, prefix: "changed" },
      }),
    ).rejects.toThrow("prefix_changed");
    const compact = await bridge.call("thread/compact/start", {
      threadId: bridge.thread,
      requestId: "idle",
      idleCompaction: report,
    });
    await bridge.note(
      (note) =>
        note.method === "turn/completed" &&
        note.params.turn.id === compact.turn.id,
    );
    expect(
      bridge.notes.filter(
        (note) => note.method === "thread/idleCompaction/updated",
      ),
    ).toHaveLength(0);
    expect(
      bridge.notes.filter(
        (note) =>
          note.method === "item/completed" &&
          note.params.item.type === "contextCompaction",
      ),
    ).toHaveLength(1);
  } finally {
    await bridge.close();
  }
});

test("compaction disable values follow Claude's environment rules", () => {
  expect(compactionEnabled({}, { DISABLE_COMPACT: "false" })).toBe(true);
  expect(compactionEnabled({}, { DISABLE_COMPACT: "0" })).toBe(true);
  expect(compactionEnabled({}, { DISABLE_COMPACT: "ON" })).toBe(false);
  expect(compactionEnabled({ autoCompactEnabled: false }, {})).toBe(false);
});
