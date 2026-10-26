// A Claude subscription limit through the real bridge with a scripted SDK. No
// model calls. The messages follow what Claude Code writes to its stream when a
// request is refused at the limit: a rate_limit_event, then a synthetic
// assistant message with `error: "rate_limit"`, then a failed result.
import { afterEach, test } from "vitest";
import assert from "node:assert/strict";
import { startBridge } from "./testing/bridge-harness.ts";

const RESET = 1_900_000_000;
const LIMIT_TEXT = "You've hit your session limit · resets 3:40am (UTC)";

// Input "work": one completed turn, then Claude starts a turn of its own (as for
// a background task notification) and the limit refuses its first request. The
// rate_limit_event comes before that turn exists. Input "plain": the turn the
// input started is refused, and the message has no rate_limit_info.
const FAKE_SDK = String.raw`
export const tool=(name,description,schema,call)=>({name,description,schema,call});
export const createSdkMcpServer=value=>({...value});
export const forkSession=async()=>({sessionId:'22222222-2222-4222-8222-222222222222'});
export const getSessionMessages=async()=>[];
const usage={input_tokens:10,output_tokens:2};
const none={input_tokens:0,output_tokens:0};
const info={status:'rejected',resetsAt:${RESET},rateLimitType:'five_hour',overageStatus:'rejected',overageDisabledReason:'org_level_disabled',isUsingOverage:false};
const refused=(id,details)=>({type:'assistant',uuid:'a-'+id,parent_tool_use_id:null,
 message:{id,type:'message',role:'assistant',model:'<synthetic>',stop_reason:'stop_sequence',usage:none,content:[{type:'text',text:${JSON.stringify(LIMIT_TEXT)}}]},
 error:'rate_limit',is_api_error_message:true,api_error_status:429,
 ...(details?{api_error:'usage_limit_reached',api_error_params:{rate_limit_info:info}}:{})});
const failed={type:'result',subtype:'success',is_error:true,usage:none,result:${JSON.stringify(LIMIT_TEXT)}};
export function query({prompt,options}){
 const abort=new AbortController();
 return {
  mcpServerStatus:async()=>[],reconnectMcpServer:async()=>{},
  supportedModels:async()=>[{value:'default',displayName:'Default',resolvedModel:'claude-opus-5-5'}],
  accountInfo:async()=>({email:'test@example.test',subscriptionType:'Claude Max',apiProvider:'firstParty'}),
  initializationResult:async()=>({commands:[]}),
  setModel:async()=>{},setPermissionMode:async()=>{},applyFlagSettings:async()=>{},stopTask:async()=>{},
  setMcpServers:async()=>({added:[],removed:[],errors:{}}),
  close(){abort.abort();},interrupt:async()=>abort.abort(),
  async *[Symbol.asyncIterator](){
   const input=await prompt.next();
   if(input.done)return;
   const mode=input.value.message.content[0].text;
   yield {type:'user',isReplay:true,uuid:input.value.uuid};
   yield {type:'system',subtype:'init'};
   if(mode==='plain'){
    yield refused('refused-1',false);
    yield failed;
   }else{
    yield {type:'stream_event',event:{type:'message_start',message:{id:'message-1'}}};
    yield {type:'assistant',uuid:'a-1',message:{id:'message-1',model:'claude-opus-5-5',usage,content:[{type:'text',text:'started'}]}};
    yield {type:'result',subtype:'success',usage,result:'started'};
    yield {type:'rate_limit_event',rate_limit_info:info,uuid:'r-1'};
    yield refused('refused-2',true);
    yield failed;
   }
   await new Promise(resolve=>abort.signal.addEventListener('abort',resolve,{once:true}));
  }
 };
}
`;

const bridges = [];
afterEach(async () => {
  for (const bridge of bridges.splice(0)) await bridge.close();
});

async function start(text) {
  const bridge = await startBridge(FAKE_SDK, "claude-usage-limit-");
  bridges.push(bridge);
  const turn = (
    await bridge.call("turn/start", {
      threadId: bridge.thread,
      clientUserMessageId: "limit-" + text,
      input: [{ type: "text", text }],
    })
  ).turn.id;
  return { bridge, turn };
}

test("a limit that refuses a turn Claude started itself reports the provider reset", async () => {
  const { bridge, turn } = await start("work");
  const first = await bridge.note(
    (n) => n.method === "turn/completed" && n.params.turn.id === turn,
  );
  assert.equal(first.params.turn.status, "completed");
  const own = await bridge.note(
    (n) => n.method === "turn/completed" && n.params.turn.id !== turn,
  );
  assert.equal(own.params.turn.status, "failed");
  assert.deepEqual(own.params.turn.error, {
    message: LIMIT_TEXT,
    codexErrorInfo: "rateLimitExceeded",
    resetsAt: RESET,
  });
  const shown = await bridge.note(
    (n) =>
      n.method === "item/completed" &&
      n.params.turnId === own.params.turn.id &&
      n.params.item.id ===
        "limit:" + own.params.turn.id + ":five_hour:" + RESET,
  );
  assert.match(
    shown.params.item.text,
    new RegExp(
      "^Claude usage limit reached \\(five_hour\\)\\. Resets at " +
        new Date(RESET * 1000).toISOString().replaceAll(".", "\\."),
    ),
  );
});

test("a limit without rate-limit details still reports a usage wait", async () => {
  const { bridge, turn } = await start("plain");
  const done = await bridge.note(
    (n) => n.method === "turn/completed" && n.params.turn.id === turn,
  );
  assert.equal(done.params.turn.status, "failed");
  assert.deepEqual(done.params.turn.error, {
    message: LIMIT_TEXT,
    codexErrorInfo: "rateLimitExceeded",
  });
});
