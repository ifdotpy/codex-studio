// Moving a running Bash call to the background through the real bridge.
// The scripted SDK follows the Claude Code task protocol; no model calls.
import { afterEach, test } from "vitest";
import assert from "node:assert/strict";
import fs from "node:fs/promises";
import { existsSync } from "node:fs";
import path from "node:path";
import { startBridge } from "./testing/bridge-harness.mjs";
import {
  backgroundedCommandFields,
  foregroundCommandUpdate,
  moveCommandToBackground,
} from "./command-background.mjs";

// Turn "bash": Claude runs one Bash call. After `.registered` exists, the call
// registers as a foreground local_bash task. `backgroundTasks` records each call
// in `.background-calls`; a matching call releases the turn with Claude's
// background tool result. `.finish` ends the background task later.
const FAKE_SDK = String.raw`
import fs from 'node:fs';
export const tool=(name,description,schema,call)=>({name,description,schema,call});
export const createSdkMcpServer=value=>({...value});
export const forkSession=async()=>({sessionId:'22222222-2222-4222-8222-222222222222'});
export const getSessionMessages=async()=>[];
const until=(file,signal)=>waitForFile(file,{signal}).catch(()=>{});
const usage={input_tokens:10,output_tokens:2};
export function query({prompt,options}){
 const abort=new AbortController(),cwd=options.cwd;
 let moved,movedPromise=new Promise(resolve=>moved=resolve),foreground=false;
 return {
  mcpServerStatus:async()=>[],reconnectMcpServer:async()=>{},
  supportedModels:async()=>[{value:'default',displayName:'Default',resolvedModel:'claude-opus-5-5'}],
  accountInfo:async()=>({email:'test@example.test',subscriptionType:'Claude Max',apiProvider:'firstParty'}),
  initializationResult:async()=>({commands:[]}),
  setModel:async()=>{},setPermissionMode:async()=>{},applyFlagSettings:async()=>{},stopTask:async()=>{},
  setMcpServers:async()=>({added:[],removed:[],errors:{}}),
  backgroundTasks:async toolUseId=>{
   fs.appendFileSync(cwd+'/.background-calls',String(toolUseId)+'\n');
   if(!foreground||toolUseId!=='toolu-1')return false;
   foreground=false;moved();return true;
  },
  close(){abort.abort();},interrupt:async()=>abort.abort(),
  async *[Symbol.asyncIterator](){
   const input=await prompt.next();
   if(input.done)return;
   yield {type:'user',isReplay:true,uuid:input.value.uuid};
   yield {type:'system',subtype:'init'};
   yield {type:'stream_event',event:{type:'message_start',message:{id:'message-1'}}};
   yield {type:'assistant',uuid:'a-1',message:{id:'message-1',model:'claude-opus-5-5',usage,content:[{type:'tool_use',id:'toolu-1',name:'Bash',input:{command:'sleep 600'}}]}};
   await until(cwd+'/.registered',abort.signal);
   if(abort.signal.aborted)return;
   foreground=true;
   yield {type:'system',subtype:'task_started',task_id:'bash-1',tool_use_id:'toolu-1',task_type:'local_bash',is_backgrounded:false,description:'sleep 600',uuid:'s-1'};
   await movedPromise;
   yield {type:'system',subtype:'task_updated',task_id:'bash-1',patch:{is_backgrounded:true},uuid:'s-2'};
   yield {type:'user',uuid:'u-1',message:{role:'user',content:[{type:'tool_result',tool_use_id:'toolu-1',content:'Command running in background with ID: bash-1. Output is being written to: /tmp/bash-1.output'}]},tool_use_result:{stdout:'',stderr:'',interrupted:false,backgroundTaskId:'bash-1',backgroundedByUser:true}};
   yield {type:'result',subtype:'success',usage,result:'continued'};
   await until(cwd+'/.finish',abort.signal);
   if(abort.signal.aborted)return;
   yield {type:'system',subtype:'task_notification',task_id:'bash-1',tool_use_id:'toolu-1',status:'completed',output_file:'/tmp/bash-1.output',summary:'Background command completed (exit code 0)',uuid:'s-3'};
   await new Promise(resolve=>abort.signal.addEventListener('abort',resolve,{once:true}));
  }
 };
}
`;

const bridges = [];
afterEach(async () => {
  for (const bridge of bridges.splice(0)) await bridge.close();
});

async function runningCommand() {
  const bridge = await startBridge(FAKE_SDK, "claude-background-");
  bridges.push(bridge);
  const turn = (
    await bridge.call("turn/start", {
      threadId: bridge.thread,
      clientUserMessageId: "start-bash",
      input: [{ type: "text", text: "bash" }],
    })
  ).turn.id;
  await bridge.note(
    (n) => n.method === "item/started" && n.params.item.id === "toolu-1",
  );
  return { bridge, turn };
}

const move = (bridge, turnId, itemId = "toolu-1") =>
  bridge.call("thread/commandExecution/background", {
    threadId: bridge.thread,
    turnId,
    itemId,
  });

async function backgroundCalls(bridge) {
  const file = path.join(bridge.root, ".background-calls");
  return existsSync(file)
    ? (await fs.readFile(file, "utf8")).trim().split("\n")
    : [];
}

test("a registered foreground command moves to the background and releases the turn", async () => {
  const { bridge, turn } = await runningCommand();
  await assert.rejects(move(bridge, turn), /cannot move to the background yet/);
  await fs.writeFile(path.join(bridge.root, ".registered"), "");
  const offered = await bridge.note(
    (n) =>
      n.method === "item/started" &&
      n.params.item.id === "toolu-1" &&
      n.params.item.canMoveToBackground === true,
  );
  assert.equal(offered.params.turnId, turn);
  assert.deepEqual(await move(bridge, turn), { status: "backgrounded" });
  const done = await bridge.note(
    (n) => n.method === "item/completed" && n.params.item.id === "toolu-1",
  );
  assert.equal(done.params.item.status, "completed");
  assert.equal(done.params.item.backgroundTaskId, "bash-1");
  assert.equal(done.params.item.backgroundedByUser, true);
  assert.equal(done.params.item.canMoveToBackground, false);
  assert.equal("exitCode" in done.params.item, false);
  const completed = await bridge.note(
    (n) => n.method === "turn/completed" && n.params.turn.id === turn,
  );
  assert.equal(completed.params.turn.status, "completed");
  // A retry after the move cannot reach Claude again or repeat the command.
  await assert.rejects(move(bridge, turn), /different Claude turn/);
  assert.deepEqual(await backgroundCalls(bridge), ["toolu-1"]);
  // Claude reports the end of the background task as its own item.
  await fs.writeFile(path.join(bridge.root, ".finish"), "");
  const finished = await bridge.note(
    (n) =>
      n.method === "item/completed" &&
      n.params.item.id === "task:bash-1" &&
      n.params.item.status === "completed",
  );
  assert.match(
    finished.params.item.result.content[0].text,
    /Background command completed/,
  );
});

test("a request for another turn or item never reaches Claude", async () => {
  const { bridge, turn } = await runningCommand();
  await fs.writeFile(path.join(bridge.root, ".registered"), "");
  await bridge.note(
    (n) =>
      n.method === "item/started" && n.params.item.canMoveToBackground === true,
  );
  await assert.rejects(move(bridge, "other-turn"), /different Claude turn/);
  await assert.rejects(
    move(bridge, turn, "unknown-item"),
    /Select a running command/,
  );
  assert.deepEqual(await backgroundCalls(bridge), []);
  assert.deepEqual(await move(bridge, turn), { status: "backgrounded" });
  assert.deepEqual(await backgroundCalls(bridge), ["toolu-1"]);
});

test("task events set the control only for a running foreground shell", () => {
  const item = {
    id: "toolu-2",
    type: "commandExecution",
    status: "inProgress",
  };
  const active = { tools: new Map([["toolu-2", item]]) };
  const started = {
    subtype: "task_started",
    task_type: "local_bash",
    tool_use_id: "toolu-2",
    is_backgrounded: false,
  };
  assert.equal(foregroundCommandUpdate(active, started, started), item);
  assert.equal(item.canMoveToBackground, true);
  // An unchanged flag publishes nothing.
  assert.equal(foregroundCommandUpdate(active, started, started), null);
  const agent = { ...started, task_type: "local_agent" };
  assert.equal(foregroundCommandUpdate(active, agent, agent), null);
  const moved = {
    subtype: "task_updated",
    patch: { is_backgrounded: true },
  };
  assert.equal(
    foregroundCommandUpdate(active, { ...started, ...moved }, moved),
    item,
  );
  assert.equal(item.canMoveToBackground, false);
  const background = { ...started, is_backgrounded: true };
  assert.equal(foregroundCommandUpdate(active, background, background), null);
});

test("only a backgrounded result drops the exit code", () => {
  assert.equal(backgroundedCommandFields({ stdout: "done" }), null);
  assert.equal(backgroundedCommandFields(undefined), null);
  assert.deepEqual(backgroundedCommandFields({ backgroundTaskId: "bash-3" }), {
    canMoveToBackground: false,
    backgroundTaskId: "bash-3",
    backgroundedByUser: false,
  });
});

test("an SDK without the control fails visibly", async () => {
  const active = {
    turn: { id: "turn" },
    q: {},
    tools: new Map([
      [
        "toolu-4",
        {
          type: "commandExecution",
          status: "inProgress",
          canMoveToBackground: true,
        },
      ],
    ]),
  };
  await assert.rejects(
    moveCommandToBackground(active, { turnId: "turn", itemId: "toolu-4" }),
    /cannot move a command to the background/,
  );
  active.q.backgroundTasks = async () => false;
  assert.deepEqual(
    await moveCommandToBackground(active, {
      turnId: "turn",
      itemId: "toolu-4",
    }),
    { status: "not_running" },
  );
});
