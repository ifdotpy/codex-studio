import { afterEach, test } from "vitest";
import assert from "node:assert/strict";
import fs from "node:fs/promises";
import path from "node:path";
import { startBridge } from "./testing/bridge-harness.mjs";

const SDK = String.raw`
import {appendFileSync,mkdirSync,writeFileSync,copyFileSync} from 'node:fs';
import path from 'node:path';
const messages = new Map();
export const tool=(name,description,schema,call)=>({name,description,schema,call});
export const createSdkMcpServer=value=>({...value});
export const forkSession=async()=>{throw new Error('first-turn rollback must not fork');};
export const getSessionMessages=async(id)=>messages.get(id)||[];
export function query({prompt,options}){
 const log=(value)=>appendFileSync(path.join(options.cwd,'sdk-calls.jsonl'),JSON.stringify(value)+'\n');
 log({kind:'query',enabled:options.enableFileCheckpointing,env:options.env.CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING,resume:options.resume});
 const abort=new AbortController();
 return {
  mcpServerStatus:async()=>[],reconnectMcpServer:async()=>{},supportedModels:async()=>[],
  accountInfo:async()=>({email:'test@example.test',subscriptionType:'Claude Max',apiProvider:'firstParty'}),
  initializationResult:async()=>({commands:[]}),setModel:async()=>{},setPermissionMode:async()=>{},applyFlagSettings:async()=>{},stopTask:async()=>{},
  setMcpServers:async()=>({added:[],removed:[],errors:{}}),
  rewindFiles:async(messageId,rewind)=>{log({kind:'rewind',messageId,rewind,resume:options.resume});if(!rewind.dryRun)copyFileSync(path.join(process.env.CLAUDE_CONFIG_DIR,'file-history',options.resume,'0123456789abcdef@v1'),path.join(options.cwd,'file.txt'));return {canRewind:true,filesChanged:['file.txt']};},
  close(){abort.abort();},interrupt:async()=>abort.abort(),
  async *[Symbol.asyncIterator](){
   const input=await prompt.next();
   if(input.done)return;
   messages.set(options.sessionId,[{type:'user',uuid:input.value.uuid,message:input.value.message,parent_tool_use_id:null}]);
   const config=process.env.CLAUDE_CONFIG_DIR;
   const project=path.join(config,'projects',options.cwd.replace(/[^a-zA-Z0-9]/g,'-'));
   mkdirSync(project,{recursive:true});
   writeFileSync(path.join(project,options.sessionId+'.jsonl'),JSON.stringify({type:'user',uuid:input.value.uuid,parentUuid:null,sessionId:options.sessionId,cwd:options.cwd,isSidechain:false,timestamp:new Date().toISOString(),message:input.value.message})+'\n');
   mkdirSync(path.join(config,'file-history',options.sessionId),{recursive:true});
   writeFileSync(path.join(config,'file-history',options.sessionId,'0123456789abcdef@v1'),'checkpoint');
   writeFileSync(path.join(options.cwd,'file.txt'),'later user edit');
   appendFileSync(path.join(project,options.sessionId+'.jsonl'),JSON.stringify({type:'file-history-snapshot',messageId:input.value.uuid,snapshot:{messageId:input.value.uuid,trackedFileBackups:{'file.txt':{backupFileName:'0123456789abcdef@v1',version:1}}}})+'\n');
   log({kind:'input',messageId:input.value.uuid});
   yield {type:'system',subtype:'init',claude_code_version:'fixture'};
   yield {type:'user',isReplay:true,uuid:input.value.uuid};
   yield {type:'result',subtype:'success',usage:{input_tokens:1,output_tokens:1},result:'done'};
   await new Promise(resolve=>abort.signal.addEventListener('abort',resolve,{once:true}));
  }
 };
}
`;
const bridges = [];
afterEach(async () => {
  for (const bridge of bridges.splice(0)) await bridge.close();
});

test("bridge enables SDK checkpoints and rewinds the exact echoed message with context", async () => {
  const bridge = await startBridge(SDK, "claude-file-checkpoints-");
  bridges.push(bridge);
  const started = await bridge.call("turn/start", {
    threadId: bridge.thread,
    clientUserMessageId: "human-message",
    input: [{ type: "text", text: "edit" }],
  });
  await bridge.note(
    (n) =>
      n.method === "turn/completed" && n.params.turn.id === started.turn.id,
  );
  const state = await bridge.call("claude/state", { threadId: bridge.thread });
  assert.equal(state.turns[0].canRestoreFiles, true);
  const request = {
    threadId: bridge.thread,
    turnId: started.turn.id,
    requestId: "rewind",
    restoreFiles: true,
  };
  const result = await bridge.call("thread/rollback", request);
  assert.equal(result.filesRestored, true);
  assert.equal(
    await fs.readFile(path.join(bridge.root, "file.txt"), "utf8"),
    "checkpoint",
  );
  assert.equal(
    (await bridge.call("claude/state", { threadId: bridge.thread })).turns
      .length,
    0,
  );
  assert.deepEqual(await bridge.call("thread/rollback", request), result);
  const usage = await bridge.call("claude/fileBackups", {
    threadId: bridge.thread,
  });
  assert.equal(usage.count, 1);
  assert.equal(
    (
      await bridge.call("claude/fileBackups", {
        threadId: bridge.thread,
        settledRequestIds: [],
      })
    ).released,
    0,
  );
  const released = await bridge.call("claude/fileBackups", {
    threadId: bridge.thread,
    settledRequestIds: ["rewind"],
  });
  assert.equal(released.released, 1);
  assert.equal(released.count, 0);
  assert.deepEqual(await bridge.call("thread/rollback", request), result);
  assert.equal(
    await fs.readFile(path.join(bridge.root, "file.txt"), "utf8"),
    "checkpoint",
  );
  const calls = (
    await fs.readFile(path.join(bridge.root, "sdk-calls.jsonl"), "utf8")
  )
    .trim()
    .split("\n")
    .map(JSON.parse);
  const input = calls.find((c) => c.kind === "input");
  for (const q of calls.filter((c) => c.kind === "query")) {
    assert.equal(q.enabled, true);
    assert.equal(q.env, "true");
  }
  const rewinds = calls.filter((c) => c.kind === "rewind");
  assert.equal(rewinds.length, 2);
  assert.deepEqual(
    rewinds.map((c) => c.rewind),
    [{ dryRun: true }, { dryRun: false }],
  );
  assert.ok(
    rewinds.every(
      (c) => c.messageId === input.messageId && c.resume === bridge.thread,
    ),
  );
});
