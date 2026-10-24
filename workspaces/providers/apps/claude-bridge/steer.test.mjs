// Busy-turn steer delivery through the real bridge with a scripted SDK.
// No model calls: the fake query echoes, reads or withholds input on cue.
import { afterEach, test } from "vitest";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import fs from "node:fs/promises";
import { existsSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import { createInterface } from "node:readline";
import { fileURLToPath } from "node:url";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const HANG_GUARD_MS = 10_000;

// Scenario by the turn's first input text:
// ack: read the steer and echo it inside the turn.
// pulled: read the steer without an echo (outcome unknown).
// late: read the steer, end the turn with an error, then echo the steer.
// dequeue: read the steer without an echo, finish the reply, then answer the
// steer as its own prompt; each result names the input it answered, as the
// Claude CLI does when it dequeues a steer instead of folding it in.
// stuck: the same as dequeue, but the results name no input (an older CLI),
// so the bridge keeps the turn open; the transcript records both answers.
// anything else: never read further input until `.end` exists.
const FAKE_SDK = String.raw`
export const resolveSettings=async()=>({effective:{}});
import fs from 'node:fs';
export const tool=(name,description,schema,call)=>({name,description,schema,call});
export const createSdkMcpServer=value=>({...value});
export const forkSession=async()=>({sessionId:'22222222-2222-4222-8222-222222222222'});
export const getSessionMessages=async()=>[];
const until=async(file,signal)=>{while(!fs.existsSync(file)&&!signal.aborted)await new Promise(r=>setTimeout(r,5));};
const usage={input_tokens:10,output_tokens:2};
export function query({prompt,options}){
 const abort=new AbortController(),cwd=options.cwd;
 let nativeQueue=[],stopTurn=null,queueMode=false,survive=false;
 const read=async()=>{const input=await prompt.next();if(!input.done){fs.appendFileSync(cwd+'/.inputs',input.value.message.content[0].text+'\n');fs.appendFileSync(cwd+'/.sdk-inputs',JSON.stringify(input.value)+'\n');}return input;};
 return {
  mcpServerStatus:async()=>[],reconnectMcpServer:async()=>{},
  supportedModels:async()=>[{value:'default',displayName:'Default',resolvedModel:'claude-opus-5-5'}],
  accountInfo:async()=>({email:'test@example.test',subscriptionType:'Claude Max',apiProvider:'firstParty'}),
  initializationResult:async()=>({commands:[]}),
  setModel:async()=>{},setPermissionMode:async()=>{},applyFlagSettings:async()=>{},stopTask:async(id)=>{fs.appendFileSync(cwd+'/.stopped-tasks',id+'\n');if(fs.existsSync(cwd+'/.task-failure'))throw new Error('Task stop failed');},
  setMcpServers:async()=>({added:[],removed:[],errors:{}}),
  close(){abort.abort();},interrupt:async(options)=>{
   if(!queueMode){abort.abort();return;}
   fs.writeFileSync(cwd+'/.interrupt-options',JSON.stringify(options??null));
   const cancelled=options?.cancelQueued&&!survive?nativeQueue.splice(0).map(m=>m.uuid):[];
   const receipt={cancelled,still_queued:nativeQueue.map(m=>m.uuid)};
   fs.writeFileSync(cwd+'/.native-interrupt-receipt',JSON.stringify(receipt));
   stopTurn();return receipt;
  },
  async *[Symbol.asyncIterator](){
   while(!abort.signal.aborted){
    const input=await read();
    if(input.done)return;
    const text=input.value.message.content[0].text;
    yield {type:'user',isReplay:true,uuid:input.value.uuid};
    if(input.value.shouldQuery===false){const fail=text==='prefix-fail';yield {type:'result',subtype:fail?'error_during_execution':'success',is_error:fail,errors:fail?['prefix failed']:undefined,num_turns:0,user_message_uuid:input.value.uuid,usage};continue;}
    yield {type:'system',subtype:'init'};
    if(text.startsWith('native-'))yield {type:'system',subtype:'background_tasks_changed',tasks:[{task_id:'background-1'}]};
    yield {type:'stream_event',event:{type:'message_start',message:{id:'message-'+text}}};
    if(text.startsWith('native-')){
     queueMode=true;survive=text.endsWith('survivor');
     const stopped=new Promise(r=>stopTurn=r);
     for(;;){const next=await read();nativeQueue.push(next.value);if(text!=='native-split-all'||next.value.shouldQuery!==false)break;}
     fs.writeFileSync(cwd+'/.native-queued','');
     await stopped;
     yield {type:'result',subtype:'error_during_execution',is_error:true,usage,errors:['interrupted']};
     for(const held of nativeQueue.splice(0)){
      fs.writeFileSync(cwd+'/.survived','');
      yield {type:'user',isReplay:true,uuid:held.uuid};
     }
     queueMode=false;continue;
    }
    if(text==='dequeue'){
     const next=await read();
     yield {type:'result',subtype:'success',usage,result:'first',user_message_uuid:input.value.uuid,user_message_uuids:[input.value.uuid]};
     yield {type:'stream_event',event:{type:'message_start',message:{id:'message-steer'}}};
     yield {type:'result',subtype:'success',usage,result:'second',user_message_uuid:next.value.uuid,user_message_uuids:[next.value.uuid]};
     continue;
    }
    if(text==='stuck'){
     const next=await read();
     const session=input.value.session_id;
     const dir=process.env.CLAUDE_CONFIG_DIR+'/projects/'+cwd.replace(/[^a-zA-Z0-9]/g,'-');
     fs.mkdirSync(dir,{recursive:true});
     const row=fields=>JSON.stringify({sessionId:session,cwd,isSidechain:false,...fields})+'\n';
     const first=crypto.randomUUID(),second=crypto.randomUUID();
     fs.writeFileSync(dir+'/'+session+'.jsonl',
      row({type:'user',uuid:input.value.uuid,parentUuid:null,message:{role:'user',content:[{type:'text',text}]}})+
      row({type:'assistant',uuid:first,parentUuid:input.value.uuid,message:{id:'message-stuck',role:'assistant',content:[{type:'text',text:'first'}],stop_reason:'end_turn'}})+
      row({type:'user',uuid:next.value.uuid,parentUuid:first,message:{role:'user',content:[{type:'text',text:'steer text'}]}})+
      row({type:'assistant',uuid:second,parentUuid:next.value.uuid,message:{id:'message-answer',role:'assistant',content:[{type:'text',text:'second'}],stop_reason:'end_turn'}}));
     yield {type:'result',subtype:'success',usage,result:'first'};
     yield {type:'result',subtype:'success',usage,result:'second'};
     fs.writeFileSync(cwd+'/.stuck','');
     continue;
    }
    let late=null;
    if(text==='ack'){for(;;){const next=await read();yield {type:'user',isReplay:true,uuid:next.value.uuid};if(next.value.shouldQuery!==false)break;yield {type:'result',subtype:'success',num_turns:0,user_message_uuid:next.value.uuid,usage};}}
    if(text==='pulled')await read();
    if(text==='late')late=await read();
    await until(cwd+'/.end',abort.signal);
    if(abort.signal.aborted)return;
    fs.rmSync(cwd+'/.end');
    if(late||fs.existsSync(cwd+'/.fail'))yield {type:'result',subtype:'error_during_execution',is_error:true,usage,errors:['Claude failed']};
    else yield {type:'result',subtype:'success',usage,result:'done'};
    if(late)yield {type:'user',isReplay:true,uuid:late.value.uuid};
   }
  }
 };
}
`;

const bridges = [];
afterEach(async () => {
  for (const bridge of bridges.splice(0)) await bridge.close();
});

async function startBridge(env = {}) {
  // The native history reader refuses a path behind a symlink (macOS /var).
  const root = await fs.realpath(
    await fs.mkdtemp(path.join(os.tmpdir(), "claude-steer-")),
  );
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
        CLAUDE_CONFIG_DIR: path.join(root, "native-config"),
        ...env,
      },
    },
  );
  let stderr = "";
  child.stderr.on("data", (chunk) => (stderr += chunk));
  const notes = [];
  const waiting = new Map();
  let wake = () => {};
  createInterface({ input: child.stdout }).on("line", (line) => {
    const message = JSON.parse(line);
    if (message.method) notes.push(message);
    else waiting.get(message.id)?.(message);
    wake();
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
      if (message.error)
        throw Object.assign(new Error(message.error.message), {
          data: message.error.data,
        });
      return message.result;
    },
    async note(predicate) {
      const deadline = Date.now() + HANG_GUARD_MS;
      for (;;) {
        const found = notes.find(predicate);
        if (found) return found;
        if (Date.now() > deadline)
          throw new Error("notification missing\n" + stderr);
        await new Promise((resolve) => {
          wake = resolve;
          setTimeout(resolve, 50);
        });
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
    await bridge.call("thread/start", { cwd: root, dynamicTools: [] })
  ).thread.id;
  return bridge;
}

const text = (value) => [{ type: "text", text: value }];
const completed = (bridge, turn) =>
  bridge.note(
    (n) => n.method === "turn/completed" && n.params.turn.id === turn,
  );
const ack = (bridge, id) =>
  bridge.notes.find(
    (n) =>
      n.method === "item/completed" &&
      n.params.item.type === "userMessage" &&
      n.params.item.clientId === id,
  );

async function running(bridge, scenario) {
  const turn = (
    await bridge.call("turn/start", {
      threadId: bridge.thread,
      clientUserMessageId: "start-" + scenario,
      input: text(scenario),
    })
  ).turn.id;
  await bridge.note(
    (n) =>
      n.method === "item/started" ||
      (n.method === "turn/started" && n.params.turn.id === turn),
  );
  return turn;
}

const steer = (bridge, turn, id, value = "steer text") =>
  bridge.call("turn/steer", {
    threadId: bridge.thread,
    expectedTurnId: turn,
    clientUserMessageId: id,
    input: text(value),
  });

async function end(bridge, fail = false) {
  if (fail) await fs.writeFile(path.join(bridge.root, ".fail"), "");
  await fs.writeFile(path.join(bridge.root, ".end"), "");
}

async function inputs(bridge) {
  const file = path.join(bridge.root, ".inputs");
  return existsSync(file)
    ? (await fs.readFile(file, "utf8")).trim().split("\n")
    : [];
}

test("an echoed steer is acknowledged once inside its running turn", async () => {
  const bridge = await startBridge();
  const turn = await running(bridge, "ack");
  assert.deepEqual(await steer(bridge, turn, "event-1"), { turnId: turn });
  const delivered = await bridge.note((n) => ack(bridge, "event-1") === n);
  assert.equal(delivered.params.turnId, turn);
  assert.equal(delivered.params.item.id, "event-1");
  assert.deepEqual(delivered.params.item.content, text("steer text"));
  // A retried request with the same identity is not written again.
  assert.deepEqual(await steer(bridge, turn, "event-1"), { turnId: turn });
  await end(bridge);
  const done = await completed(bridge, turn);
  assert.equal(done.params.turn.status, "completed");
  assert.equal(done.params.turn.unappliedSteers, undefined);
  assert.deepEqual(await inputs(bridge), ["ack", "steer text"]);
  assert.equal(
    bridge.notes.filter((n) => ack(bridge, "event-1") === n).length,
    1,
  );
});

// Live 24 October 2026: Claude finished its reply, then answered a steer as
// its own prompt without a replay echo. The bridge deferred both results while
// the steer stayed pending, so the turn never ended, Studio kept it running,
// and every later message waited.
test("a steer that Claude answers as its own prompt is delivered and ends the turn", async () => {
  const bridge = await startBridge();
  const turn = await running(bridge, "dequeue");
  assert.deepEqual(await steer(bridge, turn, "event-dequeued"), {
    turnId: turn,
  });
  const done = await completed(bridge, turn);
  assert.equal(done.params.turn.status, "completed");
  assert.equal(done.params.turn.unappliedSteers, undefined);
  const delivered = ack(bridge, "event-dequeued");
  assert.ok(delivered, "the steer is acknowledged");
  assert.equal(delivered.params.turnId, turn);
  assert.equal(
    bridge.notes.filter((n) => ack(bridge, "event-dequeued") === n).length,
    1,
  );
  assert.deepEqual(await inputs(bridge), ["dequeue", "steer text"]);
});

test("a steer for another turn is rejected before any input is written", async () => {
  const bridge = await startBridge();
  const turn = await running(bridge, "hold");
  await assert.rejects(
    steer(bridge, "other-turn", "event-2"),
    /different Claude turn/,
  );
  await end(bridge);
  await completed(bridge, turn);
  await assert.rejects(steer(bridge, turn, "event-3"), /different Claude turn/);
  assert.deepEqual(await inputs(bridge), ["hold"]);
  assert.equal(ack(bridge, "event-2"), undefined);
});

test("a turn that fails before reading a steer withdraws and reports it", async () => {
  const bridge = await startBridge();
  const turn = await running(bridge, "hold");
  await steer(bridge, turn, "event-4");
  await end(bridge, true);
  const done = await completed(bridge, turn);
  assert.equal(done.params.turn.status, "failed");
  assert.deepEqual(done.params.turn.unappliedSteers, ["event-4"]);
  // The persistent query must not read the withdrawn input as a later turn.
  await fs.rm(path.join(bridge.root, ".fail"));
  const next = await running(bridge, "probe");
  await end(bridge);
  await completed(bridge, next);
  assert.deepEqual(await inputs(bridge), ["hold", "probe"]);
  assert.equal(ack(bridge, "event-4"), undefined);
});

test("an interrupt before Claude reads a steer reports it unapplied", async () => {
  const bridge = await startBridge();
  const turn = await running(bridge, "hold");
  await steer(bridge, turn, "event-5");
  await bridge.call("turn/interrupt", {
    threadId: bridge.thread,
    turnId: turn,
  });
  const done = await completed(bridge, turn);
  assert.equal(done.params.turn.status, "interrupted");
  assert.deepEqual(done.params.turn.unappliedSteers, ["event-5"]);
  assert.deepEqual(await inputs(bridge), ["hold"]);
});

async function read(bridge, value) {
  const deadline = Date.now() + HANG_GUARD_MS;
  while (!(await inputs(bridge)).includes(value)) {
    if (Date.now() > deadline) throw new Error(value + " was not read");
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
}

test("a steer read without an echo is neither acknowledged nor reported unapplied", async () => {
  const bridge = await startBridge();
  const turn = await running(bridge, "pulled");
  await steer(bridge, turn, "event-6");
  await read(bridge, "steer text");
  await bridge.call("turn/interrupt", {
    threadId: bridge.thread,
    turnId: turn,
  });
  const done = await completed(bridge, turn);
  assert.equal(done.params.turn.status, "interrupted");
  assert.equal(done.params.turn.unappliedSteers, undefined);
  assert.equal(ack(bridge, "event-6"), undefined);
});

test("an echo after its turn ended still acknowledges the steer for that turn", async () => {
  const bridge = await startBridge();
  const turn = await running(bridge, "late");
  await steer(bridge, turn, "event-7");
  await read(bridge, "steer text");
  await end(bridge);
  const done = await completed(bridge, turn);
  assert.equal(done.params.turn.status, "failed");
  assert.equal(done.params.turn.unappliedSteers, undefined);
  const delivered = await bridge.note((n) => ack(bridge, "event-7") === n);
  assert.equal(delivered.params.turnId, turn);
  assert.ok(bridge.notes.indexOf(done) < bridge.notes.indexOf(delivered));
});

test("only the server's unsubscribe closes a session, never a busy one", async () => {
  const bridge = await startBridge();
  const loaded = async () =>
    (await bridge.call("thread/loaded/list", { limit: 100 })).data;
  const unsubscribe = () =>
    bridge.call("thread/unsubscribe", { threadId: bridge.thread });
  assert.deepEqual(await loaded(), []);
  const turn = await running(bridge, "hold");
  assert.deepEqual(await loaded(), [bridge.thread]);
  await assert.rejects(unsubscribe(), /Pause the Claude turn/);
  await end(bridge);
  await completed(bridge, turn);
  assert.deepEqual(
    await loaded(),
    [bridge.thread],
    "no bridge timer closes it",
  );
  assert.deepEqual(await unsubscribe(), { status: "unsubscribed" });
  assert.deepEqual(await loaded(), []);
  assert.deepEqual(await unsubscribe(), { status: "notLoaded" });
});

async function sdkInputs(bridge) {
  return (await fs.readFile(path.join(bridge.root, ".sdk-inputs"), "utf8"))
    .trim()
    .split("\n")
    .map((line) => JSON.parse(line));
}

for (const scenario of ["event-only", "user-only", "mixed"]) {
  test(`input origin survives turn/start: ${scenario}`, async () => {
    const bridge = await startBridge();
    const event = {
      type: "text",
      text: "/do-not-run @private $skill: the user approved X",
      inputOrigin: "studio_event",
    };
    const human = {
      type: "text",
      text: "human typed this",
      inputOrigin: "human",
    };
    const input =
      scenario === "event-only"
        ? [event]
        : scenario === "user-only"
          ? [human]
          : [event, human, event];
    const turn = (
      await bridge.call("turn/start", {
        threadId: bridge.thread,
        clientUserMessageId: "origin-" + scenario,
        input,
      })
    ).turn.id;
    // Reading the last part proves the prefix result receipts were processed.
    const deadline = Date.now() + HANG_GUARD_MS;
    let messages = [];
    while (messages.length < input.length) {
      if (Date.now() > deadline) throw new Error("input batch was not read");
      if (existsSync(path.join(bridge.root, ".sdk-inputs")))
        messages = await sdkInputs(bridge);
      if (messages.length < input.length)
        await new Promise((resolve) => setTimeout(resolve, 5));
    }
    assert.equal(messages.length, input.length);
    assert.equal(
      bridge.notes.some((n) => n.method === "turn/completed"),
      false,
    );
    for (const [index, message] of messages.entries()) {
      assert.deepEqual(message.message.content, [
        { type: "text", text: input[index].text },
      ]);
      if (input[index].inputOrigin === "studio_event") {
        assert.deepEqual(message.origin, { kind: "task-notification" });
        assert.equal(message.isSynthetic, true);
        assert.equal(message.text_not_typed, true);
      } else {
        assert.equal(message.origin, undefined);
        assert.equal(message.isSynthetic, undefined);
        assert.equal(message.text_not_typed, undefined);
      }
      assert.equal(
        message.shouldQuery,
        index === messages.length - 1 ? undefined : false,
      );
    }
    assert.equal(new Set(messages.map((m) => m.uuid)).size, messages.length);
    await end(bridge);
    assert.equal(
      (await completed(bridge, turn)).params.turn.status,
      "completed",
    );
  });
}

for (const origins of [
  ["studio_event"],
  ["human"],
  ["studio_event", "human", "studio_event"],
]) {
  test(`input origin survives turn/steer: ${origins.join(",")}`, async () => {
    const bridge = await startBridge();
    const turn = await running(bridge, "ack");
    const input = origins.map((inputOrigin, index) => ({
      type: "text",
      text: "part-" + index,
      inputOrigin,
    }));
    await bridge.call("turn/steer", {
      threadId: bridge.thread,
      expectedTurnId: turn,
      clientUserMessageId: "origin-steer",
      input,
    });
    await bridge.note((n) => ack(bridge, "origin-steer") === n);
    assert.equal(
      bridge.notes.some((n) => n.method === "turn/completed"),
      false,
    );
    const messages = (await sdkInputs(bridge)).slice(1);
    assert.equal(messages.length, input.length);
    for (const [index, message] of messages.entries()) {
      const synthetic = origins[index] === "studio_event";
      assert.deepEqual(
        message.origin,
        synthetic ? { kind: "task-notification" } : undefined,
      );
      assert.equal(message.isSynthetic, synthetic ? true : undefined);
      assert.equal(message.text_not_typed, synthetic ? true : undefined);
    }
    await end(bridge);
    assert.equal(
      (await completed(bridge, turn)).params.turn.status,
      "completed",
    );
  });
}

test("a failed split prefix closes its query without submitting the remaining parts", async () => {
  const bridge = await startBridge();
  const turn = (
    await bridge.call("turn/start", {
      threadId: bridge.thread,
      clientUserMessageId: "prefix-failure",
      input: [
        { type: "text", text: "prefix-fail", inputOrigin: "studio_event" },
        { type: "text", text: "must not be submitted", inputOrigin: "human" },
      ],
    })
  ).turn.id;
  assert.equal((await completed(bridge, turn)).params.turn.status, "failed");
  const messages = await sdkInputs(bridge);
  assert.equal(messages.length, 1);
  assert.equal(messages[0].message.content[0].text, "prefix-fail");
});

async function nativeQueued(bridge) {
  const deadline = Date.now() + HANG_GUARD_MS;
  while (!existsSync(path.join(bridge.root, ".native-queued"))) {
    if (Date.now() > deadline)
      throw new Error("Native queue did not receive input");
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
}

test("Stop cancels input held in Claude's native queue without closing the query", async () => {
  const bridge = await startBridge();
  const turn = await running(bridge, "native-queue");
  await steer(bridge, turn, "studio:correction");
  await nativeQueued(bridge);
  const receipt = await bridge.call("turn/interrupt", {
    threadId: bridge.thread,
    turnId: turn,
  });
  assert.deepEqual(
    JSON.parse(
      await fs.readFile(path.join(bridge.root, ".interrupt-options"), "utf8"),
    ),
    { cancelQueued: true },
  );
  assert.deepEqual(receipt, {
    cancelled: ["studio:correction"],
    still_queued: [],
  });
  await completed(bridge, turn);
  assert.equal(
    await fs.readFile(path.join(bridge.root, ".stopped-tasks"), "utf8"),
    "background-1\n",
  );
  const next = await running(bridge, "probe");
  await end(bridge);
  await completed(bridge, next);
  assert.equal(existsSync(path.join(bridge.root, ".survived")), false);
  assert.equal(ack(bridge, "studio:correction"), undefined);
});

test("Stop returns surviving native input with its exact Studio identity", async () => {
  const bridge = await startBridge();
  const turn = await running(bridge, "native-survivor");
  await steer(bridge, turn, "studio:survivor");
  await nativeQueued(bridge);
  const receipt = await bridge.call("turn/interrupt", {
    threadId: bridge.thread,
    turnId: turn,
    stopBackgroundTasks: false,
  });
  assert.deepEqual(receipt, {
    cancelled: [],
    still_queued: ["studio:survivor"],
  });
  assert.equal(existsSync(path.join(bridge.root, ".stopped-tasks")), false);
});

test("Stop without an active turn leaves the query unchanged", async () => {
  const bridge = await startBridge();
  const turn = await running(bridge, "hold");
  await end(bridge);
  await completed(bridge, turn);
  await assert.rejects(
    bridge.call("turn/interrupt", { threadId: bridge.thread }),
    /no active turn/,
  );
  const next = await running(bridge, "probe");
  await end(bridge);
  await completed(bridge, next);
  assert.deepEqual(await inputs(bridge), ["hold", "probe"]);
});

test("a background task error preserves the successful queue cancellation receipt", async () => {
  const bridge = await startBridge();
  const turn = await running(bridge, "native-queue");
  await steer(bridge, turn, "studio:cancel-with-task-error");
  await nativeQueued(bridge);
  await fs.writeFile(path.join(bridge.root, ".task-failure"), "");
  await assert.rejects(
    bridge.call("turn/interrupt", { threadId: bridge.thread, turnId: turn }),
    (error) => {
      assert.equal(error.message, "Task stop failed");
      assert.deepEqual(error.data.interruptReceipt, {
        cancelled: ["studio:cancel-with-task-error"],
        still_queued: [],
      });
      return true;
    },
  );
});

for (const scenario of [
  "native-split-all",
  "native-split-tail",
  "native-split-survivor",
]) {
  test(`Stop settles a mixed input atomically: ${scenario}`, async () => {
    const bridge = await startBridge();
    const turn = await running(bridge, scenario);
    const identity = "studio:mixed-stop";
    const input = ["studio_event", "human", "studio_event"].map(
      (inputOrigin, index) => ({
        type: "text",
        text: `mixed-part-${index}`,
        inputOrigin,
      }),
    );
    await bridge.call("turn/steer", {
      threadId: bridge.thread,
      expectedTurnId: turn,
      clientUserMessageId: identity,
      input,
    });
    await nativeQueued(bridge);
    const beforeStop = (await sdkInputs(bridge)).slice(1);
    assert.equal(beforeStop.length, scenario === "native-split-all" ? 3 : 1);
    assert.equal(beforeStop[0].shouldQuery, false);
    const receipt = await bridge.call("turn/interrupt", {
      threadId: bridge.thread,
      turnId: turn,
    });
    const survivor = scenario === "native-split-survivor";
    assert.deepEqual(receipt, {
      cancelled: survivor ? [] : [identity],
      still_queued: survivor ? [identity] : [],
    });
    const nativeReceipt = JSON.parse(
      await fs.readFile(
        path.join(bridge.root, ".native-interrupt-receipt"),
        "utf8",
      ),
    );
    assert.equal(
      nativeReceipt.cancelled.length,
      survivor ? 0 : beforeStop.length,
    );
    assert.equal(nativeReceipt.still_queued.length, survivor ? 1 : 0);
    await completed(bridge, turn);
    const next = await running(bridge, "probe");
    await end(bridge);
    await completed(bridge, next);
    assert.equal(ack(bridge, identity), undefined);
    assert.equal(existsSync(path.join(bridge.root, ".survived")), survivor);
    const afterStop = (await sdkInputs(bridge)).slice(1 + beforeStop.length);
    assert.deepEqual(
      afterStop.map((message) => message.message.content[0].text),
      ["probe"],
    );
  });
}

// Studio's liveness check (turn_stalled): Claude ended the turn, but the
// bridge kept it open because the results named no steer. Native history
// shows the final answer and records the steer, so the turn completes once,
// the steer is delivered once, and nothing is sent to Claude again.
test("a turn that Claude ended while a steer kept it open completes once from native history", async () => {
  const bridge = await startBridge({ STUDIO_CLAUDE_RECONCILE_QUIET_MS: "1" });
  const turn = await running(bridge, "stuck");
  assert.deepEqual(await steer(bridge, turn, "event-stuck"), { turnId: turn });
  const deadline = Date.now() + HANG_GUARD_MS;
  while (!existsSync(path.join(bridge.root, ".stuck"))) {
    assert.ok(Date.now() < deadline, "the scripted results arrive");
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
  await new Promise((resolve) => setTimeout(resolve, 50));
  assert.equal(
    bridge.notes.some(
      (n) => n.method === "turn/completed" && n.params.turn.id === turn,
    ),
    false,
    "the bridge holds the turn open",
  );
  const reconcile = () =>
    bridge.call("turn/reconcile", { threadId: bridge.thread, turnId: turn });
  assert.deepEqual(await reconcile(), { status: "completed" });
  const done = await completed(bridge, turn);
  assert.equal(done.params.turn.status, "completed");
  assert.equal(done.params.turn.unappliedSteers, undefined);
  assert.equal(
    bridge.notes.filter((n) => ack(bridge, "event-stuck") === n).length,
    1,
    "the steer is delivered once",
  );
  assert.deepEqual(await reconcile(), { status: "notActive" });
  assert.deepEqual(await inputs(bridge), ["stuck", "steer text"]);
});

test("a turn that native history does not show ended stays open", async () => {
  const bridge = await startBridge({ STUDIO_CLAUDE_RECONCILE_QUIET_MS: "1" });
  const turn = await running(bridge, "hold");
  await new Promise((resolve) => setTimeout(resolve, 20));
  assert.deepEqual(
    await bridge.call("turn/reconcile", {
      threadId: bridge.thread,
      turnId: turn,
    }),
    { status: "unconfirmed" },
  );
  await end(bridge);
  const done = await completed(bridge, turn);
  assert.equal(done.params.turn.status, "completed");
  assert.deepEqual(await inputs(bridge), ["hold"]);
});
