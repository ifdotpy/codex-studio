// Preserve the Stop receipt contracts after removal of the Python bridge tests.
import { afterEach, test } from "vitest";
import assert from "node:assert/strict";
import fs from "node:fs/promises";
import { existsSync } from "node:fs";
import path from "node:path";
import { startBridge, HANG_GUARD_MS } from "./testing/bridge-harness.mjs";

const SDK = String.raw`
import fs from 'node:fs';
import fsp from 'node:fs/promises';
const wait=async(file)=>{while(!fs.existsSync(file))await new Promise(r=>setTimeout(r,5));};
const append=fsp.appendFile;
fsp.appendFile=async(file,data,...args)=>{
 const result=await append(file,data,...args);
 const root=process.cwd();
 if(fs.existsSync(root+'/.hold-save')&&String(data).includes('preparing')&&!fs.existsSync(root+'/.save-held')){
  fs.writeFileSync(root+'/.save-held','');
  await wait(root+'/.release-save');
 }
 return result;
};
export const tool=(name,description,schema,call)=>({name,description,schema,call});
export const createSdkMcpServer=value=>({...value});
export const forkSession=async()=>({sessionId:'22222222-2222-4222-8222-222222222222'});
export const getSessionMessages=async()=>[];
export function query({prompt,options}){
 const cwd=options.cwd,abort=new AbortController();
 return {
  mcpServerStatus:async()=>[],reconnectMcpServer:async()=>{},
  supportedModels:async()=>[{value:'default',displayName:'Default',resolvedModel:'claude-opus-5-5'}],
  accountInfo:async()=>{
   if(options.systemPrompt&&fs.existsSync(cwd+'/.hold-account')){
    fs.writeFileSync(cwd+'/.account-entered','');
    await wait(cwd+'/.release-account');
   }
   return {email:'test@example.test',subscriptionType:'Claude Max',apiProvider:'firstParty'};
  },
  initializationResult:async()=>({commands:[]}),
  setModel:async()=>{},setPermissionMode:async()=>{},applyFlagSettings:async()=>{},stopTask:async()=>{},
  setMcpServers:async()=>({added:[],removed:[],errors:{}}),
  close(){abort.abort();},interrupt:async()=>({cancelled:[],still_queued:[]}),
  async *[Symbol.asyncIterator](){
   while(!abort.signal.aborted){
    const input=await prompt.next();if(input.done)return;
    fs.appendFileSync(cwd+'/.admitted',input.value.uuid+'\n');
    yield {type:'user',isReplay:true,uuid:input.value.uuid};
    yield {type:'system',subtype:'init'};
    yield {type:'result',subtype:'success',usage:{input_tokens:1,output_tokens:1},result:'done'};
   }
  }
 };
}
`;
const bridges = [];
afterEach(async () => {
  for (const bridge of bridges.splice(0)) {
    // Release injected barriers even if an assertion fails.
    await fs.writeFile(path.join(bridge.root, ".release-save"), "");
    await fs.writeFile(path.join(bridge.root, ".release-account"), "");
    await bridge.close();
  }
});
async function fixture() {
  const bridge = await startBridge(SDK, "claude-stop-receipts-");
  bridges.push(bridge);
  return bridge;
}
const start = (bridge, id) =>
  bridge.call("turn/start", {
    threadId: bridge.thread,
    clientUserMessageId: id,
    input: [{ type: "text", text: id }],
  });
const completed = async (bridge, id) =>
  (
    await bridge.note(
      (n) => n.method === "turn/completed" && n.params.turn.id === id,
    )
  ).params.turn;
async function marker(bridge, name) {
  const file = path.join(bridge.root, name);
  const deadline = Date.now() + HANG_GUARD_MS;
  while (!existsSync(file)) {
    assert.ok(Date.now() < deadline, `Missing ${name}`);
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
}
const admissions = async (bridge) =>
  existsSync(path.join(bridge.root, ".admitted"))
    ? (await fs.readFile(path.join(bridge.root, ".admitted"), "utf8"))
        .trim()
        .split("\n")
    : [];

test("Stop replies before a reserved input save completes and the rejection survives restart", async () => {
  const bridge = await fixture();
  const warm = await start(bridge, "warm");
  assert.equal((await completed(bridge, warm.turn.id)).status, "completed");
  await fs.writeFile(path.join(bridge.root, ".hold-save"), "");
  const pending = start(bridge, "blocked-save").then(
    (value) => ({ value }),
    (error) => ({ error }),
  );
  await marker(bridge, ".save-held");
  const receipt = await bridge.call("turn/interrupt", {
    threadId: bridge.thread,
  });
  assert.deepEqual(receipt, { cancelled: [], still_queued: [] });
  assert.equal(existsSync(path.join(bridge.root, ".release-save")), false);
  assert.equal((await admissions(bridge)).length, 1);
  await fs.writeFile(path.join(bridge.root, ".release-save"), "");
  const result = await pending;
  assert.match(result.error?.message ?? "", /stopped before accepting/);
  const history = await bridge.call("thread/turns/list", {
    threadId: bridge.thread,
  });
  assert.equal(history.data.at(-1).startOutcome, "not_applied");
  await bridge.restart();
  const retry = await start(bridge, "blocked-save");
  assert.equal((await completed(bridge, retry.turn.id)).status, "completed");
  assert.equal((await start(bridge, "blocked-save")).turn.id, retry.turn.id);
  assert.equal((await admissions(bridge)).length, 2);
});

test("Stop preserves a rejected pre-admission receipt and permits one exact retry after restart", async () => {
  const bridge = await fixture();
  await fs.writeFile(path.join(bridge.root, ".hold-account"), "");
  const original = await start(bridge, "before-admission");
  await marker(bridge, ".account-entered");
  const receipt = await bridge.call("turn/interrupt", {
    threadId: bridge.thread,
    turnId: original.turn.id,
  });
  assert.deepEqual(receipt, {
    cancelled: ["before-admission"],
    still_queued: [],
  });
  await fs.writeFile(path.join(bridge.root, ".release-account"), "");
  const stopped = await completed(bridge, original.turn.id);
  assert.equal(stopped.status, "interrupted");
  assert.equal(stopped.startOutcome, "not_applied");
  assert.equal(stopped.error.data.turnStartOutcome, "not_applied");
  assert.deepEqual(await admissions(bridge), []);
  await fs.rm(path.join(bridge.root, ".hold-account"));
  await bridge.restart();
  const saved = await bridge.call("thread/turns/list", {
    threadId: bridge.thread,
  });
  assert.equal(saved.data[0].startOutcome, "not_applied");
  const retry = await start(bridge, "before-admission");
  assert.notEqual(retry.turn.id, original.turn.id);
  assert.equal((await completed(bridge, retry.turn.id)).status, "completed");
  assert.equal(
    (await start(bridge, "before-admission")).turn.id,
    retry.turn.id,
  );
  assert.equal((await admissions(bridge)).length, 1);
});
