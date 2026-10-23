import { afterEach, test } from "vitest";
import assert from "node:assert/strict";
import { startBridge } from "./testing/bridge-harness.mjs";

const bridges = [];
afterEach(async () => {
  for (const bridge of bridges.splice(0)) await bridge.close();
});
function sdk(events) {
  return `
export const tool=(name,description,schema,call)=>({name,description,schema,call});
export const createSdkMcpServer=value=>({...value});
export const forkSession=async()=>({sessionId:'22222222-2222-4222-8222-222222222222'});
export const getSessionMessages=async()=>[];
export function query({prompt}) {
 const abort=new AbortController();
 return {
  supportedModels:async()=>[{value:'sonnet',displayName:'Sonnet',resolvedModel:'claude-sonnet-4-6'}],
  accountInfo:async()=>({email:'test@example.test',subscriptionType:'Claude Max',apiProvider:'firstParty'}),
  initializationResult:async()=>({commands:[]}),mcpServerStatus:async()=>[],
  setModel:async()=>{},setPermissionMode:async()=>{},applyFlagSettings:async()=>{},
  setMcpServers:async()=>({added:[],removed:[],errors:{}}),
  close(){abort.abort();},interrupt:async()=>abort.abort(),
  async *[Symbol.asyncIterator]() {
   const input=await prompt.next(); if(input.done)return;
   yield {type:'user',isReplay:true,uuid:input.value.uuid};
   yield {type:'system',subtype:'init'};
   for(const event of ${JSON.stringify(events)})yield event;
   await new Promise(resolve=>abort.signal.addEventListener('abort',resolve,{once:true}));
  }
 };
}`;
}
async function run(events, model = "sonnet") {
  const bridge = await startBridge(sdk(events), "claude-usage-");
  bridges.push(bridge);
  await bridge.call("turn/start", {
    threadId: bridge.thread,
    model,
    input: [{ type: "text", text: "Continue" }],
  });
  const done = await bridge.note((n) => n.method === "turn/completed");
  return { bridge, turn: done.params.turn };
}
const failed = {
  type: "result",
  subtype: "error_during_execution",
  is_error: true,
  errors: ["429 Too Many Requests"],
};

test("plain 429 without a rate limit event has a typed terminal cause", async () => {
  const { turn } = await run([
    {
      type: "assistant",
      error: "rate_limit",
      uuid: "error",
      message: {
        id: "error",
        model: "claude-sonnet-4-6",
        content: [{ type: "text", text: "429 Too Many Requests" }],
      },
    },
    failed,
  ]);
  assert.equal(turn.status, "failed");
  assert.equal(turn.error.codexErrorInfo, "rateLimitExceeded");
});

test("terminal result with only a plain HTTP 429 has a typed cause", async () => {
  const { turn } = await run([failed]);
  assert.equal(turn.error.codexErrorInfo, "rateLimitExceeded");
});

test("retry and fallback details survive as readable thread items", async () => {
  const { bridge } = await run([
    {
      type: "system",
      subtype: "api_retry",
      uuid: "retry-1",
      attempt: 2,
      max_retries: 5,
      retry_delay_ms: 1500,
      error_status: 429,
      error: "rate_limit",
    },
    {
      type: "system",
      subtype: "model_fallback",
      uuid: "fallback-1",
      original_model: "claude-opus-4-6",
      fallback_model: "claude-sonnet-4-6",
      content: "Changed model after the usage limit.",
    },
    {
      type: "system",
      subtype: "model_consent_fallback",
      uuid: "consent-1",
      original_model: "claude-opus-4-6",
      fallback_model: "claude-sonnet-4-6",
      choice: "session",
      content: "Changed model for this session.",
    },
    { type: "result", subtype: "success", result: "Done" },
  ]);
  const read = await bridge.call("thread/read", {
    threadId: bridge.thread,
    includeTurns: true,
  });
  const items = read.thread.turns.flatMap((turn) => turn.items);
  const retry = items.find((item) => item.id === "retry-1");
  assert.ok(retry, "retry item persisted");
  assert.match(retry.text, /2.*5/);
  assert.match(retry.text, /1500 ms/);
  assert.match(retry.text, /429/);
  for (const id of ["fallback-1", "consent-1"]) {
    const item = items.find((item) => item.id === id);
    assert.ok(item, id + " persisted");
    assert.match(item.text, /claude-opus-4-6/);
    assert.match(item.text, /claude-sonnet-4-6/);
    assert.ok(
      bridge.notes.some(
        (n) => n.method === "item/completed" && n.params.item.id === id,
      ),
    );
  }
});

test("an Opus rejection does not attach its reset to a Sonnet failure", async () => {
  const { turn } = await run([
    {
      type: "rate_limit_event",
      rate_limit_info: {
        rateLimitType: "seven_day_opus",
        status: "rejected",
        utilization: 1,
        resetsAt: 9000,
      },
    },
    {
      type: "result",
      subtype: "error_during_execution",
      is_error: true,
      errors: ["Unrelated failure"],
    },
  ]);
  assert.equal(turn.error.codexErrorInfo, undefined);
  assert.equal(turn.error.resetsAt, undefined);
});
