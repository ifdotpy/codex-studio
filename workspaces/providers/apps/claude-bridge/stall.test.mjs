import { waitForFile } from "./testing/file-signals.mjs";
import { startBridge as startScriptedBridge } from "./testing/bridge-harness.mjs";
// Stalled tool calls through the real bridge with a scripted SDK. The fake CLI
// emits a Bash tool_use and then never returns its result, as in the
// 2026-10-17 incident. No model calls.
import { afterEach, test } from "vitest";
import assert from "node:assert/strict";
import fs from "node:fs/promises";
import { existsSync, readFileSync } from "node:fs";
import path from "node:path";
import { bashTimeoutMs, findStall, recordedInputIds } from "./stall-watch.mjs";

const GRACE_MS = 300;

// Scenario by the turn's first input text:
// stall: a Bash call that never returns; an interrupt ends the turn, then the
//   CLI runs the steers it holds as its next turn (Claude Code's interrupt
//   keeps queued messages).
// wedged: as stall, but the CLI ignores the interrupt; it records a steer whose
//   text contains "recorded" as a queued_command in its native history.
// deaf: as wedged, and closing the query does not end its stream either.
// approval: the CLI asks canUseTool first; after an allow it stalls as in
//   "stall", after a deny it reports the denial and ends the turn.
// heartbeat: the tool sends progress past its timeout, then returns.
// answer: a reply without a tool call.
// In every scenario, the tool call starts only after the test removes its
// .hold file; until then the CLI queues the steers it reads.
const FAKE_SDK = String.raw`
export const resolveSettings=async()=>({effective:{}});
import fs from 'node:fs';
import path from 'node:path';
export const tool=(name,description,schema,call)=>({name,description,schema,call});
export const createSdkMcpServer=value=>({...value});
export const forkSession=async()=>({sessionId:'22222222-2222-4222-8222-222222222222'});
export const getSessionMessages=async()=>[];
const usage={input_tokens:10,output_tokens:2};
const sleep=ms=>new Promise(r=>setTimeout(r,ms));
export function query({prompt,options}){
 const abort=new AbortController(),cwd=options.cwd;
 const session=options.sessionId||options.resume;
 const history=path.join(process.env.CLAUDE_CONFIG_DIR,'projects',cwd.replace(/[^a-zA-Z0-9]/g,'-'),session+'.jsonl');
 let interrupted=null,wedged=false,deaf=false;
 const interrupt=new Promise(r=>(interrupted=r));
 const queued=[];
 let reading=null;
 const read=()=>reading||=prompt.next().then(input=>{reading=null;
  if(!input.done)fs.appendFileSync(cwd+'/.inputs',input.value.message.content[0].text+'\n');
  return input;});
 const tool={type:'tool_use',id:'toolu_stall',name:'Bash',input:{command:'kubectl get cc',timeout:100}};
 // Hold a steer like the CLI queue: no echo until the turn ends.
 const hold=value=>{
  queued.push(value);
  if(value.message.content[0].text.includes('recorded'))
   fs.appendFileSync(history,JSON.stringify({type:'attachment',sessionId:session,cwd,isSidechain:false,attachment:{type:'queued_command',source_uuid:value.uuid}})+'\n');
 };
 return {
  mcpServerStatus:async()=>[],reconnectMcpServer:async()=>{},
  supportedModels:async()=>[{value:'default',displayName:'Default',resolvedModel:'claude-opus-5-5'}],
  accountInfo:async()=>({email:'test@example.test',subscriptionType:'Claude Max',apiProvider:'firstParty'}),
  initializationResult:async()=>({commands:[]}),
  setModel:async()=>{},setPermissionMode:async()=>{},applyFlagSettings:async()=>{},stopTask:async()=>{},
  setMcpServers:async()=>({added:[],removed:[],errors:{}}),
  close(){if(!deaf)abort.abort();},
  interrupt(){fs.appendFileSync(cwd+'/.interrupts','x');if(wedged){if(fs.existsSync(cwd+'/.stop-now'))return Promise.resolve({still_queued:queued.map(q=>q.uuid)});return new Promise(()=>{});}interrupted();return Promise.resolve({still_queued:queued.map(q=>q.uuid)});},
  async *[Symbol.asyncIterator](){
   fs.mkdirSync(path.dirname(history),{recursive:true});
   while(!abort.signal.aborted){
    const input=await read();
    if(input.done)return;
    const text=input.value.message.content[0].text;
    fs.appendFileSync(history,JSON.stringify({type:'user',uuid:input.value.uuid,sessionId:session,cwd,isSidechain:false})+'\n');
    if(text==='stall')fs.appendFileSync(history,'broken native row\n');
    yield {type:'user',isReplay:true,uuid:input.value.uuid};
    yield {type:'system',subtype:'init'};
    yield {type:'stream_event',event:{type:'message_start',message:{id:'message-'+text}}};
    // While the test keeps a .hold file, steers queue before the tool call.
    while(fs.existsSync(cwd+'/.hold')){
     const next=await Promise.race([read(),waitForFile(cwd+'/.hold',{absent:true}).then(()=>null)]);
     if(next?.done)return;
     if(next)hold(next.value);
    }
    if(text==='answer'){
     yield {type:'assistant',uuid:'assistant-'+text,message:{id:'message-'+text,content:[{type:'text',text:'done'}],usage}};
     yield {type:'result',subtype:'success',usage,result:'done'};
     continue;
    }
    // The CLI sends its permission request beside the stream, as the SDK
    // control channel does; it does not wait for the bridge to read the tool call.
    const asked=text==='approval'?options.canUseTool('Bash',tool.input,{signal:abort.signal,toolUseID:tool.id,decisionReason:'[1mrm on a critical path[0m'}):null;
    asked?.catch(()=>{});
    yield {type:'assistant',uuid:'assistant-'+text,message:{id:'message-'+text,content:[tool],usage}};
    if(text==='heartbeat'){
     // Progress every 50 ms for the timeout plus twice the stall grace: past
     // the stall budget and the watchdog check after it, so only the progress
     // events keep the call alive.
     const until=Date.now()+tool.input.timeout+2*Number(process.env.STUDIO_CLAUDE_TOOL_STALL_GRACE_MS);
     for(let i=0;Date.now()<until;i++){await sleep(50);yield {type:'tool_progress',tool_use_id:tool.id,elapsed_time_seconds:i};}
     yield {type:'user',message:{role:'user',content:[{type:'tool_result',tool_use_id:tool.id,content:'ok'}]}};
     yield {type:'result',subtype:'success',usage,result:'done'};
     continue;
    }
    if(text==='approval'){
     const decision=await asked;
     fs.writeFileSync(cwd+'/.approved',decision.behavior);
     if(decision.behavior==='deny'){
      yield {type:'user',message:{role:'user',content:[{type:'tool_result',tool_use_id:tool.id,is_error:true,content:decision.message}]}};
      yield {type:'result',subtype:'success',usage,result:'declined'};
      continue;
     }
    }
    wedged=text==='wedged'||text==='deaf';
    deaf=text==='deaf';
    for(;;){
     const next=await Promise.race([read(),interrupt.then(()=>null),new Promise(r=>abort.signal.addEventListener('abort',()=>r(null),{once:true}))]);
     if(abort.signal.aborted)return;
     if(!next)break;
     if(next.done)return;
     hold(next.value);
    }
    yield {type:'result',subtype:'error_during_execution',is_error:true,usage,errors:['interrupted']};
    // Claude Code runs the messages that survived the interrupt next.
    for(const value of queued.splice(0)){
     yield {type:'user',isReplay:true,uuid:value.uuid};
     yield {type:'stream_event',event:{type:'message_start',message:{id:'message-after-'+value.uuid}}};
     yield {type:'result',subtype:'success',usage,result:'answered'};
    }
   }
  }
 };
}
`;

const bridges = [];
afterEach(async () => {
  for (const bridge of bridges.splice(0)) await bridge.close();
});

async function startBridge({ env = {}, thread = {} } = {}) {
  const bridge = await startScriptedBridge(FAKE_SDK, "claude-stall-", {
    env: {
      STUDIO_CLAUDE_TOOL_STALL_GRACE_MS: String(GRACE_MS),
      STUDIO_CLAUDE_STALL_INTERRUPT_WAIT_MS: "300",
      STUDIO_CLAUDE_STALL_EXIT_WAIT_MS: "100",
      ...env,
    },
    thread,
    clock: true,
  });
  bridges.push(bridge);
  return bridge;
}

const text = (value) => [{ type: "text", text: value }];
const completed = (bridge, turn) =>
  bridge.advanceUntil(
    (n) => n.method === "turn/completed" && n.params.turn.id === turn,
  );
const acks = (bridge, id) =>
  bridge.notes.filter(
    (n) =>
      n.method === "item/completed" &&
      n.params.item.type === "userMessage" &&
      n.params.item.clientId === id,
  );

// Start a scenario turn. Each steer in `steers` ([id, text] pairs) is in the
// CLI queue before its tool call starts: the stall clock starts with the tool
// call, so it cannot end the turn while a steer is still on its way.
async function start(bridge, scenario, steers = []) {
  const hold = path.join(bridge.root, ".hold");
  if (steers.length) await fs.writeFile(hold, "");
  const turn = (
    await bridge.call("turn/start", {
      threadId: bridge.thread,
      clientUserMessageId: "start-" + scenario,
      input: text(scenario),
    })
  ).turn.id;
  for (const [id, value] of steers) await steer(bridge, turn, id, value);
  if (steers.length) {
    await read(bridge, steers.at(-1)[1]);
    await fs.rm(hold);
  }
  await bridge.note(
    (n) => n.method === "item/started" && n.params.item.id === "toolu_stall",
  );
  return turn;
}

const steer = (bridge, turn, id, value) =>
  bridge.call("turn/steer", {
    threadId: bridge.thread,
    expectedTurnId: turn,
    clientUserMessageId: id,
    input: text(value),
  });

async function inputs(bridge) {
  const file = path.join(bridge.root, ".inputs");
  return existsSync(file)
    ? (await fs.readFile(file, "utf8")).trim().split("\n")
    : [];
}

const read = (bridge, value) => {
  const file = path.join(bridge.root, ".inputs");
  return waitForFile(file, {
    matches: () =>
      readFileSync(file, "utf8").trim().split("\n").includes(value),
  });
};

test("a tool stall closes a CLI with unconfirmed input without resending it", async () => {
  const bridge = await startBridge();
  const turn = await start(bridge, "stall", [
    ["late-question", "question during the stall"],
  ]);
  const done = await completed(bridge, turn);
  assert.equal(done.params.turn.status, "interrupted");
  assert.match(done.params.turn.error.message, /Bash call did not return/);
  assert.equal(done.params.turn.error.data.claudeToolStall.tool, "Bash");
  assert.equal(done.params.turn.error.data.claudeToolStall.timeoutMs, 100);
  assert.equal(done.params.turn.unappliedSteers, undefined);
  const tool = bridge.notes.find(
    (n) => n.method === "item/completed" && n.params.item.id === "toolu_stall",
  );
  assert.equal(tool.params.item.status, "failed");
  assert.equal(tool.params.item.stalled, true);
  // The CLI cannot retain ambiguous queued input after this turn. Without
  // authoritative native history it remains unknown and is never resent.
  assert.equal(acks(bridge, "late-question").length, 0);
  assert.deepEqual((await bridge.call("thread/loaded/list", {})).data, []);
  assert.deepEqual(await inputs(bridge), [
    "stall",
    "question during the stall",
  ]);
  const notice = bridge.notes.find(
    (n) => n.method === "item/completed" && n.params.item.studioNotice,
  );
  assert.match(notice.params.item.text, /Studio stopped this turn/);
  assert.notEqual(notice.params.item.phase, "final_answer");
  // The bridge accepts a new turn afterwards.
  const next = (
    await bridge.call("turn/start", {
      threadId: bridge.thread,
      clientUserMessageId: "start-after",
      input: text("answer"),
    })
  ).turn.id;
  assert.equal((await completed(bridge, next)).params.turn.status, "completed");
});

test("a CLI that ignores the interrupt is closed and its steers are settled from native history", async () => {
  const bridge = await startBridge();
  const turn = await start(bridge, "wedged", [
    ["steer-recorded", "recorded steer"],
    ["steer-lost", "steer the CLI dropped"],
  ]);
  const done = await completed(bridge, turn);
  assert.equal(done.params.turn.status, "interrupted");
  assert.match(done.params.turn.error.message, /Studio stopped this turn/);
  assert.ok(existsSync(path.join(bridge.root, ".interrupts")));
  // Recorded input is delivered; unrecorded input returns to Studio once.
  assert.deepEqual(done.params.turn.unappliedSteers, ["steer-lost"]);
  assert.equal(acks(bridge, "steer-recorded").length, 1);
  assert.equal(acks(bridge, "steer-lost").length, 0);
  // A new turn starts in a fresh CLI.
  const next = (
    await bridge.call("turn/start", {
      threadId: bridge.thread,
      clientUserMessageId: "start-fresh",
      input: text("answer"),
    })
  ).turn.id;
  assert.equal((await completed(bridge, next)).params.turn.status, "completed");
});

test("a CLI whose stream outlives its close still ends the stalled turn once", async () => {
  const bridge = await startBridge();
  const turn = await start(bridge, "deaf");
  const done = await completed(bridge, turn);
  assert.equal(done.params.turn.status, "interrupted");
  assert.match(done.params.turn.error.message, /Studio stopped this turn/);
  await bridge.advanceTime(200);
  assert.equal(
    bridge.notes.filter(
      (n) => n.method === "turn/completed" && n.params.turn.id === turn,
    ).length,
    1,
  );
  // The dropped query is replaced by a fresh CLI for the next turn.
  const next = (
    await bridge.call("turn/start", {
      threadId: bridge.thread,
      clientUserMessageId: "start-replaced",
      input: text("answer"),
    })
  ).turn.id;
  assert.equal((await completed(bridge, next)).params.turn.status, "completed");
});

test("an unanswered approval is not a stall, and the tool budget restarts at the answer", async () => {
  // An agent that asks for approvals waits for the user as long as it takes.
  const bridge = await startBridge({
    env: { STUDIO_CLAUDE_UNEXPECTED_APPROVAL_WAIT_MS: "200" },
  });
  const turn = await start(bridge, "approval");
  const approval = await bridge.note(
    (n) => n.method === "item/commandExecution/requestApproval",
  );
  assert.equal(approval.params.itemId, "toolu_stall");
  // Several tool budgets pass while Studio owns the wait.
  await bridge.advanceTime(100 + GRACE_MS * 4);
  assert.equal(
    bridge.notes.find((n) => n.method === "turn/completed"),
    undefined,
  );

  bridge.answer(approval.id, { decision: "accept" });
  const done = await completed(bridge, turn);
  assert.equal(done.params.turn.status, "interrupted");
  assert.ok(
    done.params.turn.error.data.claudeToolStall.elapsedMs >= 100 + GRACE_MS,
  );
  assert.equal(
    await fs.readFile(path.join(bridge.root, ".approved"), "utf8"),
    "allow",
  );
});

test("an approval nobody answers in a no-approval agent is declined and its card closed", async () => {
  const bridge = await startBridge({
    env: {
      STUDIO_CLAUDE_UNEXPECTED_APPROVAL_WAIT_MS: "400",
      // The decline restarts the tool budget. Keep the stall watchdog out of
      // this test, so a slow CLI answer to the decline cannot end the turn.
      STUDIO_CLAUDE_TOOL_STALL_GRACE_MS: "600000",
    },
    thread: { approvalPolicy: "never" },
  });
  const turn = await start(bridge, "approval");
  const approval = await bridge.note(
    (n) => n.method === "item/commandExecution/requestApproval",
  );
  assert.equal(
    approval.params.reason,
    "Claude requests permission to use Bash although this agent runs without approvals: rm on a critical path",
  );
  const resolved = await bridge.advanceUntil(
    (n) => n.method === "serverRequest/resolved",
  );
  assert.deepEqual(resolved.params, {
    threadId: bridge.thread,
    requestId: approval.id,
  });
  const done = await completed(bridge, turn);
  assert.equal(done.params.turn.status, "completed");
  assert.equal(
    await fs.readFile(path.join(bridge.root, ".approved"), "utf8"),
    "deny",
  );
  const notice = bridge.notes.find(
    (n) =>
      n.method === "item/completed" &&
      n.params.item.id === "approval-expired:toolu_stall",
  );
  assert.match(notice.params.item.text, /Studio declined it/);
  assert.equal(
    bridge.notes.find(
      (n) =>
        n.method === "item/completed" && n.params.item.phase === "final_answer",
    )?.params.item.studioNotice,
    undefined,
  );
  // A late answer to the closed card changes nothing.
  bridge.answer(approval.id, { decision: "accept" });
  await bridge.advanceTime(100);
  assert.equal(
    await fs.readFile(path.join(bridge.root, ".approved"), "utf8"),
    "deny",
  );
});

test("progress events keep a tool call alive past its timeout", async () => {
  // The bridge and this fake CLI share one event loop. A pause of the whole
  // process longer than the grace looks like silence, so the grace here is
  // 20 times the 50 ms progress interval.
  const bridge = await startBridge({
    env: { STUDIO_CLAUDE_TOOL_STALL_GRACE_MS: "1000" },
  });
  const turn = await start(bridge, "heartbeat");
  const done = await completed(bridge, turn);
  assert.equal(done.params.turn.status, "completed");
  assert.equal(done.params.turn.error, undefined);
  assert.equal(existsSync(path.join(bridge.root, ".interrupts")), false);
});

test("the stall rule needs both an elapsed budget and silence", () => {
  const limits = { graceMs: 100, silenceMs: 1_000 };
  const clocks = new Map([
    ["bash", { tool: "Bash", startedAt: 0, timeoutMs: 500 }],
  ]);
  assert.equal(findStall(clocks, 599, 0, limits), null);
  assert.equal(findStall(clocks, 700, 650, limits), null);
  assert.equal(findStall(clocks, 700, 600, limits).id, "bash");
  const silent = new Map([
    ["mcp", { tool: "mcp__x__y", startedAt: 0, timeoutMs: null }],
  ]);
  assert.equal(findStall(silent, 999, 0, limits), null);
  assert.equal(findStall(silent, 1_000, 0, limits).id, "mcp");
  assert.equal(bashTimeoutMs({}, {}), 120_000);
  assert.equal(bashTimeoutMs({ timeout: 9_999_999 }, {}), 600_000);
  assert.equal(
    bashTimeoutMs({}, { BASH_DEFAULT_TIMEOUT_MS: "900000" }),
    900_000,
  );
  assert.deepEqual(
    [
      ...recordedInputIds([
        JSON.stringify({ type: "user", uuid: "u1" }),
        JSON.stringify({
          type: "attachment",
          attachment: { type: "queued_command", source_uuid: "u2" },
        }),
        JSON.stringify({ type: "queue-operation", operation: "enqueue" }),
      ]),
    ],
    ["u1", "u2"],
  );
});

test("Stop bounds a running Claude that ignores its interrupt", async () => {
  const bridge = await startBridge({
    env: { STUDIO_CLAUDE_TOOL_STALL_GRACE_MS: "600000" },
  });
  const turn = await start(bridge, "deaf", [
    ["stop-recorded", "recorded steer"],
    ["stop-lost", "lost steer"],
  ]);
  await fs.writeFile(path.join(bridge.root, ".stop-now"), "");
  await bridge.call("turn/interrupt", {
    threadId: bridge.thread,
    turnId: turn,
  });
  const done = await completed(bridge, turn);
  assert.equal(done.params.turn.status, "interrupted");
  assert.match(
    done.params.turn.error.message,
    /did not end the turn after Stop/,
  );
  assert.deepEqual(done.params.turn.unappliedSteers, ["stop-lost"]);
  assert.equal(acks(bridge, "stop-recorded").length, 1);
  assert.equal(acks(bridge, "stop-lost").length, 0);
});
