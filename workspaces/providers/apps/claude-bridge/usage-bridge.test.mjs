// Real bridge protocol with SDK-declared fixtures; no model requests.
import { afterEach, expect, test } from "vitest";
import fs from "node:fs/promises";
import path from "node:path";
import { startBridge } from "./testing/bridge-harness.mjs";
import {
  events,
  modelUsage,
  sdkResult,
  zeroResult,
} from "./usage-fixtures.mjs";
const bridges = [];
afterEach(async () => {
  for (const b of bridges.splice(0)) await b.close();
});
const sdk = `
export const tool=(name,description,schema,call)=>({name,description,schema,call});
export const createSdkMcpServer=value=>({...value});
export const forkSession=async()=>({sessionId:'22222222-2222-4222-8222-222222222222'});
export const getSessionMessages=async()=>[];
const events=${JSON.stringify(events)}, modelUsage=${JSON.stringify(modelUsage)},sdkResult=${JSON.stringify(sdkResult)},zeroResult=${JSON.stringify(zeroResult)};
export function query({prompt,options}){
 const abort=new AbortController();
 const result=(zero=false)=>zero?zeroResult:{...sdkResult,modelUsage:{'claude-opus-5-5':modelUsage}};
 const stream=(event,parent=null)=>({type:'stream_event',event,parent_tool_use_id:parent});
 return {mcpServerStatus:async()=>[],reconnectMcpServer:async()=>{},supportedModels:async()=>[{value:'default',displayName:'Default',resolvedModel:'claude-opus-5-5'}],accountInfo:async()=>({email:'test@example.test',subscriptionType:'Claude Max',apiProvider:'firstParty'}),initializationResult:async()=>({commands:[]}),setModel:async()=>{},setPermissionMode:async()=>{},applyFlagSettings:async()=>{},setMcpServers:async()=>({added:[],removed:[],errors:{}}),close(){abort.abort();},interrupt:async()=>abort.abort(),
 async *[Symbol.asyncIterator](){
  while(!abort.signal.aborted){
   const input=await prompt.next();if(input.done)return;
   const text=input.value.message.content[0].text;
   yield {type:'user',isReplay:true,uuid:input.value.uuid};
   yield {type:'system',subtype:'init'};
   if(text==='steer'){
    yield stream(events[0]);
    const next=await prompt.next();
    yield result();
    yield {type:'user',isReplay:true,uuid:next.value.uuid};
    yield stream(events[1]);yield stream(events[2]);
    await waitForFile(options.cwd+'/.finish',{signal:abort.signal});
    modelUsage.outputTokens=1440;
    yield result();continue;
   }
   if(text!=='result-only'&&text!=='zero'){
    yield stream(events[0],text==='subagent'?'tool-1':null);
    if(text!=='interrupted'&&text!=='query-close'){
     yield {type:'assistant',uuid:'assistant-1',message:{id:'response-1',model:'claude-opus-5-5',usage:{input_tokens:10,output_tokens:1},content:[{type:'text',text:'answer'}]}};
     yield stream(events[1],text==='subagent'?'tool-1':null);yield stream(events[2],text==='subagent'?'tool-1':null);
    }
   }
   if(text==='query-close')return;
   if(text==='hold-final')await waitForFile(options.cwd+'/.finish',{signal:abort.signal});
   yield result(text==='zero');
  }
 }};
}`;
async function setup() {
  const b = await startBridge(sdk, "claude-usage-");
  bridges.push(b);
  return b;
}
async function start(b, text) {
  return (
    await b.call("turn/start", {
      threadId: b.thread,
      input: [{ type: "text", text }],
    })
  ).turn.id;
}
async function done(b, id) {
  await b.note((n) => n.method === "turn/completed" && n.params.turn.id === id);
}
function observations(b) {
  return b.notes
    .filter((n) => n.method === "thread/tokenUsage/updated")
    .map((n) => n.params.studioUsage);
}
test("final stream usage is emitted before turn completion and replaces provisional assistant usage", async () => {
  const b = await setup();
  const id = await start(b, "hold-final");
  await b.note((n) => n.params?.studioUsage?.kind === "response");
  expect(b.notes.some((n) => n.method === "turn/completed")).toBe(false);
  const response = observations(b).filter((o) => o?.kind === "response");
  expect(response).toHaveLength(1);
  expect(response[0]).toMatchObject({
    responseId: "response-1",
    tokens: {
      uncachedInput: 12,
      cacheReadInput: 32,
      cacheWriteInput: 42,
      output: 720,
      reasoningOutput: 20,
    },
  });
  await fs.writeFile(path.join(b.root, ".finish"), "");
  await done(b, id);
  const usageIndex = b.notes.findIndex(
    (n) => n.params?.studioUsage?.kind === "response",
  );
  expect(usageIndex).toBeLessThan(
    b.notes.findIndex((n) => n.method === "turn/completed"),
  );
});
test("every result is published before a steer echo can discard a deferred result", async () => {
  const b = await setup();
  const id = await start(b, "steer");
  await b.note((n) => n.method === "provider/generationStarted");
  await b.call("turn/steer", {
    threadId: b.thread,
    expectedTurnId: id,
    clientUserMessageId: "steer-1",
    input: [{ type: "text", text: "continue" }],
  });
  await b.note(
    (n) =>
      n.method === "item/completed" && n.params.item.clientId === "steer-1",
  );
  expect(observations(b).filter((o) => o?.kind === "cumulative")).toHaveLength(
    1,
  );
  await fs.writeFile(path.join(b.root, ".finish"), "");
  await done(b, id);
  expect(
    observations(b)
      .filter((o) => o?.kind === "cumulative")
      .map((o) => o.total.output),
  ).toEqual([720, 1440]);
});
test("equal per-turn results preserve cumulative totals and result-only turns have no stale response id", async () => {
  const b = await setup();
  await done(b, await start(b, "normal"));
  const before = b.notes.length;
  await done(b, await start(b, "result-only"));
  const notes = b.notes
    .slice(before)
    .filter((n) => n.method === "thread/tokenUsage/updated");
  expect(notes).toHaveLength(1);
  expect(notes[0].params.studioUsage).toMatchObject({
    kind: "cumulative",
    total: { output: 720 },
  });
  expect(notes[0].params.responseId).toBeUndefined();
  expect(notes[0].params.studioUsage.responseId).toBeUndefined();
});
for (const scenario of ["interrupted", "query-close"])
  test(scenario + " emits a gap rather than zero usage", async () => {
    const b = await setup();
    await done(b, await start(b, scenario));
    expect(observations(b)).toContainEqual(
      expect.objectContaining({
        kind: "gap",
        responseId: "response-1",
        reason:
          scenario === "query-close" ? "queryClosed" : "streamInterrupted",
      }),
    );
    expect(observations(b).some((o) => o?.kind === "response")).toBe(false);
  });
test("zeroed crash result does not publish an observation", async () => {
  const b = await setup();
  await done(b, await start(b, "zero"));
  expect(observations(b)).toEqual([]);
});
test("subagent stream usage retains its parent tool identity", async () => {
  const b = await setup();
  await done(b, await start(b, "subagent"));
  expect(observations(b)).toContainEqual(
    expect.objectContaining({
      kind: "response",
      parentToolUseId: "tool-1",
      tokens: expect.objectContaining({ output: 720 }),
    }),
  );
});
