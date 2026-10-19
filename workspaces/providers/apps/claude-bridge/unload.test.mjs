// Idle unload through the real bridge with a scripted SDK: a settled turn's
// thread/unsubscribe ends its CLI query, and the next turn starts a new query
// that resumes the same native session with only the new message.
import { afterEach, test } from "vitest";
import assert from "node:assert/strict";
import fs from "node:fs/promises";
import path from "node:path";
import { startBridge } from "./testing/bridge-harness.mjs";

// Every query logs its start options, its inputs and its close to the cwd.
const FAKE_SDK = String.raw`
import fs from 'node:fs';
export const tool=(name,description,schema,call)=>({name,description,schema,call});
export const createSdkMcpServer=value=>({...value});
export const forkSession=async()=>({sessionId:'22222222-2222-4222-8222-222222222222'});
export const getSessionMessages=async()=>[];
const usage={input_tokens:10,output_tokens:2};
export function query({prompt,options}){
 const abort=new AbortController(),log=(event)=>fs.appendFileSync(options.cwd+'/.log',JSON.stringify(event)+'\n');
 log({query:{resume:options.resume??null,sessionId:options.sessionId??null}});
 return {
  mcpServerStatus:async()=>[],reconnectMcpServer:async()=>{},
  supportedModels:async()=>[{value:'default',displayName:'Default',resolvedModel:'claude-opus-5-5'}],
  accountInfo:async()=>({email:'test@example.test',subscriptionType:'Claude Max',apiProvider:'firstParty'}),
  initializationResult:async()=>({commands:[]}),
  setModel:async()=>{},setPermissionMode:async()=>{},applyFlagSettings:async()=>{},stopTask:async()=>{},
  setMcpServers:async()=>({added:[],removed:[],errors:{}}),
  close(){if(!abort.signal.aborted)log({close:true});abort.abort();},interrupt:async()=>abort.abort(),
  async *[Symbol.asyncIterator](){
   while(!abort.signal.aborted){
    const input=await prompt.next();
    if(input.done)return;
    log({input:input.value.message.content[0].text});
    yield {type:'user',isReplay:true,uuid:input.value.uuid};
    yield {type:'system',subtype:'init'};
    yield {type:'result',subtype:'success',usage,result:'done'};
   }
  }
 };
}
`;

const bridges = [];
afterEach(async () => {
  for (const bridge of bridges.splice(0)) await bridge.close();
});

async function turn(bridge, text) {
  const id = (
    await bridge.call("turn/start", {
      threadId: bridge.thread,
      clientUserMessageId: "message-" + text,
      input: [{ type: "text", text }],
    })
  ).turn.id;
  return bridge.note(
    (n) => n.method === "turn/completed" && n.params.turn.id === id,
  );
}

async function log(bridge) {
  const raw = await fs.readFile(path.join(bridge.root, ".log"), "utf8");
  return raw
    .trim()
    .split("\n")
    .map((line) => JSON.parse(line));
}

test("a settled turn's unload ends its CLI and the next turn resumes the session once", async () => {
  const bridge = await startBridge(FAKE_SDK, "claude-unload-");
  bridges.push(bridge);
  const first = await turn(bridge, "first");
  assert.equal(first.params.turn.status, "completed");
  assert.deepEqual(await bridge.call("thread/loaded/list", {}), {
    data: [bridge.thread],
    nextCursor: null,
  });

  assert.deepEqual(
    await bridge.call("thread/unsubscribe", { threadId: bridge.thread }),
    { status: "unsubscribed" },
  );
  assert.deepEqual(await bridge.call("thread/loaded/list", {}), {
    data: [],
    nextCursor: null,
  });

  await bridge.call("thread/resume", { threadId: bridge.thread });
  const second = await turn(bridge, "second");
  assert.equal(second.params.turn.status, "completed");
  const events = await log(bridge);
  const [started, resumed] = events.filter((event) => event.query);
  assert.equal(started.query.resume, null);
  assert.ok(started.query.sessionId);
  assert.equal(
    resumed.query.resume,
    started.query.sessionId,
    "the new CLI resumes the same native session",
  );
  assert.deepEqual(
    events.map((event) => event.input ?? (event.close ? "close" : "query")),
    ["query", "first", "close", "query", "second"],
    "the first CLI closes before the next starts; each message is sent once",
  );
});

test("a repeated unload closes nothing more", async () => {
  const bridge = await startBridge(FAKE_SDK, "claude-unload-");
  bridges.push(bridge);
  await turn(bridge, "first");
  await bridge.call("thread/unsubscribe", { threadId: bridge.thread });
  assert.deepEqual(
    await bridge.call("thread/unsubscribe", { threadId: bridge.thread }),
    { status: "notLoaded" },
    "a repeated unload is a no-op",
  );
  assert.equal(
    (await log(bridge)).filter((event) => event.close).length,
    1,
    "only the loaded CLI was closed",
  );
});
