import { afterEach, test } from "vitest";
import assert from "node:assert/strict";
import path from "node:path";
import { createSessionStore } from "./session-store.mjs";
import { startBridge } from "./testing/bridge-harness.mjs";

const SDK = String.raw`
import fs from 'node:fs';
import fsp from 'node:fs/promises';
const rename=fsp.rename;
fsp.rename=async(from,to)=>{
 const marker=process.cwd()+'/.fail-save';
 if(to.endsWith('.meta.json')&&fs.existsSync(marker)){
  fs.rmSync(marker);throw Object.assign(new Error('fixture metadata save failed'),{code:'EIO'});
 }
 return rename(from,to);
};
export const tool=(name,description,schema,call)=>({name,description,schema,call});
export const createSdkMcpServer=value=>({...value});
export const forkSession=async()=>({sessionId:'fork'});
export const getSessionMessages=async()=>[];
export function query({prompt,options}){
 const abort=new AbortController();
 const note=(method,params)=>process.stdout.write(JSON.stringify({method,params})+'\n');
 return {
  mcpServerStatus:async()=>[],supportedModels:async()=>[],
  accountInfo:async()=>({email:'test@example.test',subscriptionType:'Claude Max',apiProvider:'firstParty'}),
  initializationResult:async()=>({commands:[]}),
  setPermissionMode:async(mode)=>{note('fixture/mode',{mode});if(mode==='plan')throw new Error('fixture live apply failed');},
  close(){abort.abort();},interrupt:async()=>abort.abort(),
  async *[Symbol.asyncIterator](){
   const input=await prompt.next();if(input.done)return;
   yield {type:'user',isReplay:true,uuid:input.value.uuid};
   yield {type:'system',subtype:'init'};
   note('fixture/options',{allowed:options.allowDangerouslySkipPermissions,mode:options.permissionMode});
   const result=await options.canUseTool('Bash',{command:'fixture'},{toolUseID:'permission-one',signal:abort.signal});
   note('fixture/permission',result);
   await waitForFile(options.cwd+'/.ask',{signal:abort.signal});
   const second=await options.canUseTool('Bash',{command:'second'},{toolUseID:'permission-two',signal:abort.signal});
   note('fixture/second-permission',second);
   await new Promise(resolve=>abort.signal.addEventListener('abort',resolve,{once:true}));
  }
 };
}
`;
const bridges = [];
afterEach(async () => {
  for (const bridge of bridges.splice(0)) await bridge.close();
});
async function running() {
  const bridge = await startBridge(SDK, "claude-live-permissions-", {
    thread: { claude: { permissionMode: "default", thinking: true } },
  });
  bridges.push(bridge);
  await bridge.call("turn/start", {
    threadId: bridge.thread,
    input: [{ type: "text", text: "wait for permission" }],
  });
  await bridge.note(
    (n) => n.method === "item/commandExecution/requestApproval",
  );
  return bridge;
}
test("a live permission switch reaches the query and Full releases its pending approval", async () => {
  const bridge = await running();
  const options = await bridge.note((n) => n.method === "fixture/options");
  assert.deepEqual(options.params, { allowed: true, mode: "default" });
  const settings = { permissionMode: "bypassPermissions", thinking: true };
  assert.deepEqual(
    await bridge.call("claude/settings", { threadId: bridge.thread, settings }),
    { settings },
  );
  assert.equal(
    (await bridge.note((n) => n.method === "fixture/mode")).params.mode,
    "bypassPermissions",
  );
  assert.equal(
    (await bridge.note((n) => n.method === "fixture/permission")).params
      .behavior,
    "allow",
  );
  assert.equal(
    (await bridge.call("claude/state", { threadId: bridge.thread })).settings
      .permissionMode,
    "bypassPermissions",
  );
  await bridge.call("claude/settings", {
    threadId: bridge.thread,
    settings: { ...settings, permissionMode: "default" },
  });
  assert.equal(
    (await bridge.call("claude/state", { threadId: bridge.thread })).settings
      .permissionMode,
    "default",
  );
  await bridge.writeSignal(".ask");
  const pending = await bridge.note(
    (n) =>
      n.method === "item/commandExecution/requestApproval" &&
      n.params.itemId === "permission-two",
  );
  assert.equal(
    bridge.notes.some((n) => n.method === "fixture/second-permission"),
    false,
  );
  bridge.process.stdin.write(
    JSON.stringify({ id: pending.id, result: { decision: "decline" } }) + "\n",
  );
  assert.equal(
    (await bridge.note((n) => n.method === "fixture/second-permission")).params
      .behavior,
    "deny",
  );
});
test("failed live apply keeps persisted settings and pending permission unchanged", async () => {
  const bridge = await running();
  await assert.rejects(
    bridge.call("claude/settings", {
      threadId: bridge.thread,
      settings: { permissionMode: "plan", thinking: true },
    }),
    (error) =>
      error.message.includes("fixture live apply failed") &&
      error.data?.settingsOutcome === "not_applied",
  );
  assert.equal(
    (await bridge.call("claude/state", { threadId: bridge.thread })).settings
      .permissionMode,
    "default",
  );
  assert.equal(
    bridge.notes.some((n) => n.method === "fixture/permission"),
    false,
  );
});

test("a metadata failure after the journal append restores persisted and live permission mode", async () => {
  const bridge = await running();
  await bridge.writeSignal(".fail-save");
  await assert.rejects(
    bridge.call("claude/settings", {
      threadId: bridge.thread,
      settings: { permissionMode: "bypassPermissions", thinking: true },
    }),
    /fixture metadata save failed/,
  );
  const stored = await createSessionStore(path.join(bridge.root, "state")).get(
    bridge.thread,
  );
  assert.equal(stored.claude.permissionMode, "default");
  assert.equal(
    bridge.notes.some((n) => n.method === "fixture/permission"),
    false,
  );
});
