// Stalled tool calls through the real bridge with a scripted SDK. The fake CLI
// emits a Bash tool_use and then never returns its result, as in the
// 2026-10-17 incident. No model calls.
import { afterEach, test } from "vitest";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import fs from "node:fs/promises";
import { existsSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import { createInterface } from "node:readline";
import { fileURLToPath } from "node:url";
import { bashTimeoutMs, findStall, recordedInputIds } from "./stall-watch.mjs";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const HANG_GUARD_MS = 10_000;
const GRACE_MS = 300;

// Scenario by the turn's first input text:
// stall: a Bash call that never returns; an interrupt ends the turn, then the
//   CLI runs the steers it holds as its next turn (Claude Code's interrupt
//   keeps queued messages).
// wedged: as stall, but the CLI ignores the interrupt; it records a steer whose
//   text contains "recorded" as a queued_command in its native history.
// approval: the CLI asks canUseTool first; after an allow it stalls as in
//   "stall", after a deny it reports the denial and ends the turn.
// heartbeat: the tool sends progress past its timeout, then returns.
const FAKE_SDK = String.raw`
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
 const history=path.join(process.env.CLAUDE_CONFIG_DIR,'projects','fake',session+'.jsonl');
 let interrupted=null,wedged=false;
 const interrupt=new Promise(r=>(interrupted=r));
 const queued=[];
 let reading=null;
 const read=()=>reading||=prompt.next().then(input=>{reading=null;
  if(!input.done)fs.appendFileSync(cwd+'/.inputs',input.value.message.content[0].text+'\n');
  return input;});
 const tool={type:'tool_use',id:'toolu_stall',name:'Bash',input:{command:'kubectl get cc',timeout:100}};
 return {
  mcpServerStatus:async()=>[],reconnectMcpServer:async()=>{},
  supportedModels:async()=>[{value:'default',displayName:'Default',resolvedModel:'claude-opus-5-5'}],
  accountInfo:async()=>({email:'test@example.test',subscriptionType:'Claude Max',apiProvider:'firstParty'}),
  initializationResult:async()=>({commands:[]}),
  setModel:async()=>{},setPermissionMode:async()=>{},applyFlagSettings:async()=>{},stopTask:async()=>{},
  setMcpServers:async()=>({added:[],removed:[],errors:{}}),
  close(){abort.abort();},
  interrupt(){fs.appendFileSync(cwd+'/.interrupts','x');if(wedged)return new Promise(()=>{});interrupted();return Promise.resolve({still_queued:queued.map(q=>q.uuid)});},
  async *[Symbol.asyncIterator](){
   fs.mkdirSync(path.dirname(history),{recursive:true});
   while(!abort.signal.aborted){
    const input=await read();
    if(input.done)return;
    const text=input.value.message.content[0].text;
    yield {type:'user',isReplay:true,uuid:input.value.uuid};
    yield {type:'system',subtype:'init'};
    yield {type:'stream_event',event:{type:'message_start',message:{id:'message-'+text}}};
    yield {type:'assistant',uuid:'assistant-'+text,message:{id:'message-'+text,content:[tool],usage}};
    if(text==='heartbeat'){
     for(let i=0;i<20;i++){await sleep(50);yield {type:'tool_progress',tool_use_id:tool.id,elapsed_time_seconds:i};}
     yield {type:'user',message:{role:'user',content:[{type:'tool_result',tool_use_id:tool.id,content:'ok'}]}};
     yield {type:'result',subtype:'success',usage,result:'done'};
     continue;
    }
    if(text==='approval'){
     const decision=await options.canUseTool('Bash',tool.input,{signal:abort.signal,toolUseID:tool.id,decisionReason:'[1mrm on a critical path[0m'});
     fs.writeFileSync(cwd+'/.approved',decision.behavior);
     if(decision.behavior==='deny'){
      yield {type:'user',message:{role:'user',content:[{type:'tool_result',tool_use_id:tool.id,is_error:true,content:decision.message}]}};
      yield {type:'result',subtype:'success',usage,result:'declined'};
      continue;
     }
    }
    wedged=text==='wedged';
    // Hold steers like the CLI queue: read them without an echo.
    for(;;){
     const next=await Promise.race([read(),interrupt.then(()=>null),new Promise(r=>abort.signal.addEventListener('abort',()=>r(null),{once:true}))]);
     if(abort.signal.aborted)return;
     if(!next)break;
     if(next.done)return;
     queued.push(next.value);
     if(next.value.message.content[0].text.includes('recorded'))
      fs.appendFileSync(history,JSON.stringify({type:'attachment',attachment:{type:'queued_command',source_uuid:next.value.uuid}})+'\n');
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
  const root = await fs.mkdtemp(path.join(os.tmpdir(), "claude-stall-"));
  for (const name of await fs.readdir(HERE)) {
    if (!name.endsWith(".mjs") || name.endsWith(".test.mjs")) continue;
    const source = await fs.readFile(path.join(HERE, name), "utf8");
    await fs.writeFile(
      path.join(root, name),
      source.replaceAll("@anthropic-ai/claude-agent-sdk", "./fake.mjs"),
    );
  }
  await fs.writeFile(path.join(root, "fake.mjs"), FAKE_SDK);
  await fs.symlink(
    path.join(HERE, "node_modules"),
    path.join(root, "node_modules"),
  );
  const child = spawn(
    process.execPath,
    [path.join(root, "bridge.mjs"), path.join(root, "state")],
    {
      cwd: root,
      stdio: ["pipe", "pipe", "pipe"],
      env: {
        ...process.env,
        STUDIO_CLAUDE_ACCOUNT: "test@example.test",
        STUDIO_CLAUDE_IDLE_SECONDS: "60",
        CLAUDE_CONFIG_DIR: path.join(root, "native-config"),
        STUDIO_CLAUDE_TOOL_STALL_GRACE_MS: String(GRACE_MS),
        STUDIO_CLAUDE_STALL_INTERRUPT_WAIT_MS: "300",
        STUDIO_CLAUDE_STALL_EXIT_WAIT_MS: "100",
        ...env,
      },
    },
  );
  let stderr = "";
  child.stderr.on("data", (chunk) => (stderr += chunk));
  const notes = [];
  const waiting = new Map();
  createInterface({ input: child.stdout }).on("line", (line) => {
    const message = JSON.parse(line);
    if (message.method) notes.push(message);
    else waiting.get(message.id)?.(message);
  });
  let sequence = 0;
  const bridge = {
    root,
    notes,
    async call(method, params) {
      const id = ++sequence;
      const reply = new Promise((resolve) => waiting.set(id, resolve));
      child.stdin.write(JSON.stringify({ id, method, params }) + "\n");
      const message = await Promise.race([
        reply,
        new Promise((_, reject) =>
          setTimeout(
            () => reject(new Error(method + " hung\n" + stderr)),
            HANG_GUARD_MS,
          ),
        ),
      ]);
      if (message.error) throw new Error(message.error.message);
      return message.result;
    },
    answer(id, result) {
      child.stdin.write(JSON.stringify({ id, result }) + "\n");
    },
    async note(predicate) {
      const deadline = Date.now() + HANG_GUARD_MS;
      for (;;) {
        const found = notes.find(predicate);
        if (found) return found;
        if (Date.now() > deadline)
          throw new Error("notification missing\n" + stderr);
        await new Promise((resolve) => setTimeout(resolve, 20));
      }
    },
    async close() {
      child.stdin.end();
      if (child.exitCode === null)
        await new Promise((resolve) => child.once("exit", resolve));
      await fs.rm(root, { recursive: true, force: true });
    },
  };
  bridges.push(bridge);
  bridge.thread = (
    await bridge.call("thread/start", {
      cwd: root,
      dynamicTools: [],
      ...thread,
    })
  ).thread.id;
  return bridge;
}

const text = (value) => [{ type: "text", text: value }];
const completed = (bridge, turn) =>
  bridge.note(
    (n) => n.method === "turn/completed" && n.params.turn.id === turn,
  );
const acks = (bridge, id) =>
  bridge.notes.filter(
    (n) =>
      n.method === "item/completed" &&
      n.params.item.type === "userMessage" &&
      n.params.item.clientId === id,
  );
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

async function start(bridge, scenario) {
  const turn = (
    await bridge.call("turn/start", {
      threadId: bridge.thread,
      clientUserMessageId: "start-" + scenario,
      input: text(scenario),
    })
  ).turn.id;
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

async function read(bridge, value) {
  const deadline = Date.now() + HANG_GUARD_MS;
  while (!(await inputs(bridge)).includes(value)) {
    if (Date.now() > deadline) throw new Error(value + " was not read");
    await sleep(5);
  }
}

test("a tool call that never returns ends the turn as a stall and keeps held steers", async () => {
  const bridge = await startBridge();
  const turn = await start(bridge, "stall");
  await steer(bridge, turn, "late-question", "question during the stall");
  await read(bridge, "question during the stall");
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
  // Claude Code runs the held steer as its next turn: delivered once, never resent.
  const delivered = await bridge.note(
    (n) => acks(bridge, "late-question")[0] === n,
  );
  assert.equal(delivered.params.turnId, turn);
  await sleep(100);
  assert.equal(acks(bridge, "late-question").length, 1);
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
      input: text("heartbeat"),
    })
  ).turn.id;
  assert.equal((await completed(bridge, next)).params.turn.status, "completed");
});

test("a CLI that ignores the interrupt is closed and its steers are settled from native history", async () => {
  const bridge = await startBridge();
  const turn = await start(bridge, "wedged");
  await steer(bridge, turn, "steer-recorded", "recorded steer");
  await steer(bridge, turn, "steer-lost", "steer the CLI dropped");
  await read(bridge, "steer the CLI dropped");
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
      input: text("heartbeat"),
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
  await sleep(100 + GRACE_MS * 4);
  assert.equal(
    bridge.notes.find((n) => n.method === "turn/completed"),
    undefined,
  );
  const answeredAt = Date.now();
  bridge.answer(approval.id, { decision: "accept" });
  const done = await completed(bridge, turn);
  assert.equal(done.params.turn.status, "interrupted");
  assert.ok(Date.now() - answeredAt >= 100 + GRACE_MS);
  assert.equal(
    await fs.readFile(path.join(bridge.root, ".approved"), "utf8"),
    "allow",
  );
});

test("an approval nobody answers in a no-approval agent is declined and its card closed", async () => {
  const bridge = await startBridge({
    env: { STUDIO_CLAUDE_UNEXPECTED_APPROVAL_WAIT_MS: "400" },
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
  const resolved = await bridge.note(
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
  await sleep(100);
  assert.equal(
    await fs.readFile(path.join(bridge.root, ".approved"), "utf8"),
    "deny",
  );
});

test("progress events keep a tool call alive past its timeout", async () => {
  const bridge = await startBridge();
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
        "not json",
      ]),
    ],
    ["u1", "u2"],
  );
});
