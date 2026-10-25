import {
  startBridge as startScriptedBridge,
  HANG_GUARD_MS,
} from "./testing/bridge-harness.mjs";
// Busy-turn steer delivery through the real bridge with a scripted SDK.
// No model calls: the fake query echoes, reads or withholds input on cue.
import { afterEach, test, vi } from "vitest";
import assert from "node:assert/strict";
import fs from "node:fs/promises";
import { existsSync } from "node:fs";
import path from "node:path";

// Scenario by the turn's first input text:
// ack: read the steer and echo it inside the turn.
// pulled: read the steer without an echo (outcome unknown).
// late: read the steer, end the turn with an error, then echo the steer.
// dequeue: read the steer without an echo, finish the reply, then answer the
// steer as its own prompt; each result names the input it answered, as the
// Claude CLI does when it dequeues a steer instead of folding it in.
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
 let nativeQueue=[],stopTurn=null,queueMode=false,survive=false,idle=false;
 const read=async()=>{const input=await prompt.next();if(!input.done){fs.appendFileSync(cwd+'/.inputs',input.value.message.content[0].text+'\n');fs.appendFileSync(cwd+'/.sdk-inputs',JSON.stringify(input.value)+'\n');}return input;};
 return {
  mcpServerStatus:async()=>[],reconnectMcpServer:async()=>{},
  supportedModels:async()=>[{value:'default',displayName:'Default',resolvedModel:'claude-opus-5-5'}],
  accountInfo:async()=>({email:'test@example.test',subscriptionType:'Claude Max',apiProvider:'firstParty'}),
  initializationResult:async()=>({commands:[]}),
  setModel:async()=>{},setPermissionMode:async()=>{},applyFlagSettings:async()=>{},stopTask:async(id)=>{fs.appendFileSync(cwd+'/.stopped-tasks',id+'\n');if(fs.existsSync(cwd+'/.task-failure'))throw new Error('Task stop failed');},
  setMcpServers:async()=>({added:[],removed:[],errors:{}}),
  close(){abort.abort();},interrupt:async(options)=>{
   if(idle)return {cancelled:[],still_queued:[]};
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
     const submitted=nativeQueue.map(message=>message.uuid);
     fs.writeFileSync(cwd+'/.native-queued','');
     await stopped;
     yield {type:'result',subtype:'error_during_execution',is_error:true,usage,errors:['interrupted']};
     if(text==='native-split-all')yield {type:'result',subtype:'success',usage,user_message_uuids:submitted};
     for(const held of nativeQueue.splice(0)){
      fs.writeFileSync(cwd+'/.survived','');
      yield {type:'user',isReplay:true,uuid:held.uuid};
     }
     queueMode=false;continue;
    }
    if(text.startsWith('dequeue')){
     let next=await read();
     yield {type:'result',subtype:'success',usage,result:'first',user_message_uuid:input.value.uuid,user_message_uuids:[input.value.uuid]};
     while(next.value.shouldQuery===false){
      if(text!=='dequeue-split-missing')yield {type:'result',subtype:'success',num_turns:0,user_message_uuid:next.value.uuid,usage};
      next=await read();
     }
     yield {type:'stream_event',event:{type:'message_start',message:{id:'message-steer'}}};
     yield {type:'result',subtype:'success',usage,result:'second',user_message_uuid:next.value.uuid,...(text==='dequeue'?{user_message_uuids:[next.value.uuid]}:{})};
     fs.writeFileSync(cwd+'/.native-queued','');
     continue;
    }
    let late=null;
    if(text==='ack'){for(;;){const next=await read();yield {type:'user',isReplay:true,uuid:next.value.uuid};if(next.value.shouldQuery!==false)break;yield {type:'result',subtype:'success',num_turns:0,user_message_uuid:next.value.uuid,usage};}}
    if(text==='pulled')await read();
    if(text==='late'||text==='success-late'||text==='grace-echo')late=await read();
    await until(cwd+'/.end',abort.signal);
    if(abort.signal.aborted)return;
    fs.rmSync(cwd+'/.end');
    if(text==='late'||fs.existsSync(cwd+'/.fail'))yield {type:'result',subtype:'error_during_execution',is_error:true,usage,errors:['Claude failed']};
    else yield {type:'result',subtype:'success',usage,result:'done'};
    idle=true;fs.writeFileSync(cwd+'/.result-sent','');
    if(text==='unread-result')await until(cwd+'/.release',abort.signal);
    if(late&&text==='late'){yield {type:'user',isReplay:true,uuid:late.value.uuid};late=null;}
    if(late){await until(cwd+'/.echo',abort.signal);if(abort.signal.aborted)return;yield {type:'user',isReplay:true,uuid:late.value.uuid};yield {type:'stream_event',event:{type:'message_start',message:{id:'continued'}}};await until(cwd+'/.continued',abort.signal);if(abort.signal.aborted)return;yield {type:'result',subtype:'success',usage,result:'continued'};}
    idle=false;
   }
  }
 };
}
`;

const bridges = [];
afterEach(async () => {
  for (const bridge of bridges.splice(0)) await bridge.close();
});

async function startBridge(env = {}, sdk = FAKE_SDK) {
  const bridge = await startScriptedBridge(sdk, "claude-steer-", {
    env: {
      STUDIO_CLAUDE_STEER_ECHO_GRACE_MS: "150",
      STUDIO_CLAUDE_STALL_EXIT_WAIT_MS: "50",
      ...env,
    },
  });
  bridges.push(bridge);
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

const ack = (bridge, id) => acks(bridge, id)[0];

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
  assert.equal(acks(bridge, "event-1").length, 1);
  bridge.notes.push(structuredClone(delivered));
  assert.equal(
    acks(bridge, "event-1").length,
    2,
    "count every matching acknowledgement",
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

for (const missing of [false, true]) {
  test(`a split steer needs exact prefix and final result receipts: missing=${missing}`, async () => {
    const bridge = await startBridge();
    const scenario = missing ? "dequeue-split-missing" : "dequeue-split";
    const turn = await running(bridge, scenario);
    const id = "split-dequeued";
    await bridge.call("turn/steer", {
      threadId: bridge.thread,
      expectedTurnId: turn,
      clientUserMessageId: id,
      input: ["studio_event", "human", "studio_event"].map(
        (inputOrigin, index) => ({
          type: "text",
          text: `split-part-${index}`,
          inputOrigin,
        }),
      ),
    });
    await nativeQueued(bridge);
    if (missing) {
      assert.equal(ack(bridge, id), undefined);
      assert.equal(
        bridge.notes.some(
          (note) =>
            note.method === "turn/completed" && note.params.turn.id === turn,
        ),
        false,
      );
    } else {
      assert.equal(
        (await completed(bridge, turn)).params.turn.status,
        "completed",
      );
      assert.ok(ack(bridge, id), "each split part has an exact result receipt");
      assert.equal(
        bridge.notes.filter((note) => ack(bridge, id) === note).length,
        1,
      );
    }
    assert.deepEqual(await inputs(bridge), [
      scenario,
      "split-part-0",
      "split-part-1",
      "split-part-2",
    ]);
  });
}

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

test("a compact active read never permits Stop to interrupt a replacement turn", async () => {
  const bridge = await startBridge();
  const previous = await running(bridge, "previous");
  await end(bridge);
  await completed(bridge, previous);
  const current = await running(bridge, "replacement");
  const snapshot = await bridge.call("thread/read", {
    threadId: bridge.thread,
    includeTurns: false,
  });
  assert.equal(snapshot.thread.status.type, "active");
  assert.deepEqual(snapshot.thread.turns, []);
  await assert.rejects(
    bridge.call("turn/interrupt", {
      threadId: bridge.thread,
      turnId: previous,
      stopBackgroundTasks: false,
    }),
    /different Claude turn/,
  );
  await end(bridge);
  assert.equal(
    (await completed(bridge, current)).params.turn.status,
    "completed",
  );
  assert.deepEqual(await inputs(bridge), ["previous", "replacement"]);
  assert.equal(existsSync(path.join(bridge.root, ".stopped-tasks")), false);
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

async function marker(bridge, name) {
  const deadline = Date.now() + HANG_GUARD_MS;
  while (!existsSync(path.join(bridge.root, name))) {
    if (Date.now() > deadline) throw new Error(name + " missing");
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
}
const release = (bridge, name) =>
  fs.writeFile(path.join(bridge.root, name), "");
const turnItems = async (bridge, turn) =>
  (
    await bridge.call("thread/read", {
      threadId: bridge.thread,
      includeTurns: true,
    })
  ).thread.turns.find((t) => t.id === turn).items;

test("a successful result without a steer echo completes without resending unknown input", async () => {
  const bridge = await startBridge();
  const turn = await running(bridge, "pulled");
  await steer(bridge, turn, "missing-echo");
  await read(bridge, "steer text");
  await end(bridge);
  const done = await completed(bridge, turn);
  assert.equal(done.params.turn.status, "completed");
  assert.equal(done.params.turn.unappliedSteers, undefined);
  assert.equal(
    (await turnItems(bridge, turn)).find((i) => i.id === "missing-echo")
      .deliveryStatus,
    "submitted",
  );
  assert.deepEqual(
    await bridge.call("thread/queue/list", { threadId: bridge.thread }),
    { data: [], items: [], nextCursor: null },
  );
  assert.deepEqual(await inputs(bridge), ["pulled", "steer text"]);
  assert.equal(ack(bridge, "missing-echo"), undefined);
});

test("Stop finalizes an idle Claude turn with a deferred successful result", async () => {
  const bridge = await startBridge({
    STUDIO_CLAUDE_STEER_ECHO_GRACE_MS: "60000",
  });
  const turn = await running(bridge, "pulled");
  await steer(bridge, turn, "stop-echo");
  await read(bridge, "steer text");
  await end(bridge);
  await marker(bridge, ".result-sent");
  await bridge.call("turn/interrupt", {
    threadId: bridge.thread,
    turnId: turn,
  });
  const done = await completed(bridge, turn);
  assert.equal(done.params.turn.status, "interrupted");
  assert.equal(done.params.turn.unappliedSteers, undefined);
});

test("an echo inside the grace keeps its continuation in the same turn", async () => {
  const bridge = await startBridge({
    STUDIO_CLAUDE_STEER_ECHO_GRACE_MS: "60000",
  });
  const turn = await running(bridge, "grace-echo");
  await steer(bridge, turn, "grace-steer");
  await read(bridge, "steer text");
  await end(bridge);
  await marker(bridge, ".result-sent");
  await release(bridge, ".echo");
  await bridge.note((n) => ack(bridge, "grace-steer") === n);
  await release(bridge, ".continued");
  assert.equal((await completed(bridge, turn)).params.turn.status, "completed");
  assert.equal(
    bridge.notes.filter((n) => n.method === "turn/started").length,
    1,
  );
});

test("a successful result withdraws unread steer once for original-identity redelivery", async () => {
  const bridge = await startBridge();
  const turn = await running(bridge, "unread-result");
  await steer(bridge, turn, "unread-steer");
  await end(bridge);
  const done = await completed(bridge, turn);
  assert.deepEqual(done.params.turn.unappliedSteers, ["unread-steer"]);
  assert.equal(
    (await turnItems(bridge, turn)).find((i) => i.id === "unread-steer")
      .deliveryStatus,
    "not_applied",
  );
  await release(bridge, ".release");
  const next = (
    await bridge.call("turn/start", {
      threadId: bridge.thread,
      clientUserMessageId: "unread-steer",
      input: text("steer text"),
    })
  ).turn.id;
  await read(bridge, "steer text");
  await end(bridge);
  await completed(bridge, next);
  assert.deepEqual(await inputs(bridge), ["unread-result", "steer text"]);
  assert.equal(
    bridge.notes.filter(
      (n) =>
        n.method === "turn/completed" &&
        n.params.turn.unappliedSteers?.includes("unread-steer"),
    ).length,
    1,
  );
});

test("Stop during a blocked final save cannot disable the next turn's Stop deadline", async () => {
  const sdk = FAKE_SDK.replace(
    "import fs from 'node:fs';",
    String.raw`
import fs from 'node:fs';
import fsp from 'node:fs/promises';
const append = fsp.appendFile;
fsp.appendFile = async(file, data, ...args) => {
 const value = await append(file, data, ...args), cwd = process.cwd();
 if(fs.existsSync(cwd+'/.hold-final-save') && String(data).includes('completed') && !fs.existsSync(cwd+'/.final-save-held')) {
  fs.writeFileSync(cwd+'/.final-save-held','');
  while(!fs.existsSync(cwd+'/.release-final-save'))await new Promise(r=>setTimeout(r,5));
 }
 return value;
};`,
  )
    .replace(
      "if(idle)return",
      "fs.writeFileSync(cwd+'/.interrupt-seen','');if(idle)return",
    )
    .replace(
      "if(!queueMode){",
      "if(fs.existsSync(cwd+'/.ignore-interrupt'))return {cancelled:[],still_queued:[]};if(!queueMode){",
    );
  const bridge = await startBridge(
    {
      STUDIO_CLAUDE_STALL_INTERRUPT_WAIT_MS: "150",
      STUDIO_CLAUDE_STALL_EXIT_WAIT_MS: "50",
    },
    sdk,
  );
  const turn = await running(bridge, "unread-result");
  await steer(bridge, turn, "save-stop");
  await release(bridge, ".hold-final-save");
  await end(bridge);
  await marker(bridge, ".final-save-held");
  const stopping = bridge.call("turn/interrupt", {
    threadId: bridge.thread,
    turnId: turn,
  });
  await marker(bridge, ".interrupt-seen");
  await release(bridge, ".release-final-save");
  await stopping;
  await completed(bridge, turn);
  await release(bridge, ".release");
  // Let the first turn's stale deadline expire before starting another turn.
  await new Promise((resolve) => setTimeout(resolve, 250));
  await release(bridge, ".ignore-interrupt");
  const next = await running(bridge, "running-ignore");
  await read(bridge, "running-ignore");
  await bridge.call("turn/interrupt", {
    threadId: bridge.thread,
    turnId: next,
  });
  const done = await completed(bridge, next);
  assert.equal(done.params.turn.status, "interrupted");
  assert.match(
    done.params.turn.error.message,
    /did not end the turn after Stop/,
  );
});

// Each fake query records an authoritative native history, independently of
// whether the SDK iterator pulled input. Only that history proves delivery.
function historySdk(mode) {
  return FAKE_SDK.replace(
    "const abort=new AbortController(),cwd=options.cwd;",
    `
 const abort=new AbortController(),cwd=options.cwd;
 const directory=process.env.CLAUDE_CONFIG_DIR+'/projects/fake';
 fs.mkdirSync(directory,{recursive:true});
 const history=directory+'/'+(options.sessionId||options.resume)+'.jsonl';
 if(!fs.existsSync(history))fs.writeFileSync(history,'');
 fs.appendFileSync(cwd+'/.queries','q');
 `,
  )
    .replace(
      "fs.appendFileSync(cwd+'/.sdk-inputs',JSON.stringify(input.value)+'\\n');",
      `fs.appendFileSync(cwd+'/.sdk-inputs',JSON.stringify(input.value)+'\\n');
 if('${mode}'==='all'||('${mode}'==='partial'&&input.value.shouldQuery===false))fs.appendFileSync(history,JSON.stringify({type:'user',uuid:input.value.uuid})+'\\n');`,
    )
    .replace(
      "close(){abort.abort();}",
      "close(){fs.writeFileSync(cwd+'/.closed','');abort.abort();}",
    );
}

test("grace closes an unconfirmed CLI before a new start can inherit its late result", async () => {
  const sdk = FAKE_SDK.replace(
    "close(){abort.abort();}",
    "close(){fs.writeFileSync(cwd+'/.closed','');abort.abort();}",
  ).replace(
    "const abort=new AbortController(),cwd=options.cwd;",
    "const abort=new AbortController(),cwd=options.cwd;fs.appendFileSync(cwd+'/.queries','q');",
  );
  const bridge = await startBridge({}, sdk);
  const old = await running(bridge, "success-late");
  await steer(bridge, old, "closed-unknown");
  await read(bridge, "steer text");
  await end(bridge);
  const done = await completed(bridge, old);
  assert.equal(done.params.turn.status, "completed");
  assert.equal(done.params.turn.unappliedSteers, undefined);
  assert.ok(existsSync(path.join(bridge.root, ".closed")));
  assert.deepEqual((await bridge.call("thread/loaded/list", {})).data, []);
  assert.equal(
    (await turnItems(bridge, old)).find((i) => i.id === "closed-unknown")
      .deliveryStatus,
    "submitted",
  );
  const next = await running(bridge, "probe");
  await read(bridge, "probe");
  await release(bridge, ".echo");
  await release(bridge, ".continued");
  await bridge.call("thread/read", {
    threadId: bridge.thread,
    includeTurns: true,
  });
  assert.equal(
    bridge.notes.some(
      (n) => n.method === "turn/completed" && n.params.turn.id === next,
    ),
    false,
  );
  await end(bridge);
  await completed(bridge, next);
  assert.equal(
    await fs.readFile(path.join(bridge.root, ".queries"), "utf8"),
    "qq",
  );
  assert.deepEqual(await inputs(bridge), [
    "success-late",
    "steer text",
    "probe",
  ]);
});

for (const mode of ["empty", "partial", "all"]) {
  test(`settings succeeds after grace-close settles a partially read batch: history=${mode}`, async () => {
    const sdk = historySdk(mode)
      .replace(
        "if(text==='pulled')await read();",
        "if(text==='pulled'){const prefix=await read();yield {type:'user',isReplay:true,uuid:prefix.value.uuid};yield {type:'result',subtype:'success',num_turns:0,user_message_uuid:prefix.value.uuid,usage};}",
      )
      .replace(
        "idle=true;fs.writeFileSync",
        "if(text==='pulled')await until(cwd+'/.allow-tail',abort.signal);idle=true;fs.writeFileSync",
      );
    const bridge = await startBridge({}, sdk);
    const old = await running(bridge, "pulled");
    await bridge.call("turn/steer", {
      threadId: bridge.thread,
      expectedTurnId: old,
      clientUserMessageId: "split-close",
      input: [
        { type: "text", text: "event-prefix", inputOrigin: "studio_event" },
        { type: "text", text: "human-tail", inputOrigin: "human" },
      ],
    });
    await read(bridge, "event-prefix");
    // For the delivered case both parts reach native history before the result.
    // The other cases leave the human tail inside InputQueue.pending.
    if (mode === "all") {
      // The fixture only pulled the prefix, so explicitly record the final UUID
      // as native history can (the SDK echo is independently withheld).
      const item = (await turnItems(bridge, old)).find(
        (i) => i.id === "split-close",
      );
      const directory = path.join(
        bridge.root,
        "native-config",
        "projects",
        "fake",
      );
      const [file] = await fs.readdir(directory);
      await fs.appendFile(
        path.join(directory, file),
        JSON.stringify({ type: "user", uuid: item.nativeId }) + "\n",
      );
    }
    await end(bridge);
    const done = await completed(bridge, old);
    const settings = { thinking: { type: "enabled", budgetTokens: 1000 } };
    assert.deepEqual(
      await bridge.call("claude/settings", {
        threadId: bridge.thread,
        settings,
      }),
      { settings },
    );
    assert.deepEqual((await bridge.call("thread/loaded/list", {})).data, []);
    const item = (await turnItems(bridge, old)).find(
      (i) => i.id === "split-close",
    );
    assert.equal(
      item.deliveryStatus,
      mode === "empty"
        ? "not_applied"
        : mode === "all"
          ? "delivered"
          : "submitted",
    );
    assert.deepEqual(
      done.params.turn.unappliedSteers,
      mode === "empty" ? ["split-close"] : undefined,
    );
    assert.equal(
      bridge.notes.filter(
        (n) =>
          n.method === "turn/completed" &&
          n.params.turn.unappliedSteers?.includes("split-close"),
      ).length,
      mode === "empty" ? 1 : 0,
    );
    assert.deepEqual(await inputs(bridge), ["pulled", "event-prefix"]);
  });
}

for (const echoed of [false, true]) {
  test(`turn/start retry preserves a completed auto-steer receipt: echoed=${echoed}`, async () => {
    const bridge = await startBridge();
    const turn = await running(bridge, echoed ? "ack" : "pulled");
    const request = {
      threadId: bridge.thread,
      clientUserMessageId: "retry-followup",
      input: text("followup"),
    };
    const first = await bridge.call("turn/start", request);
    assert.equal(first.steered, true);
    await read(bridge, "followup");
    if (echoed) await bridge.note((n) => ack(bridge, "retry-followup") === n);
    await end(bridge);
    await completed(bridge, turn);
    assert.deepEqual(await bridge.call("turn/start", request), {
      turn: { id: turn, status: "completed" },
      steered: true,
    });
    await assert.rejects(
      bridge.call("turn/start", { ...request, input: text("changed") }),
      /different content/,
    );
    assert.deepEqual(await inputs(bridge), [
      echoed ? "ack" : "pulled",
      "followup",
    ]);
  });
}

test("backpressured output waits for the prior turn's final save", async () => {
  const sdk = FAKE_SDK.replace(
    "import fs from 'node:fs';",
    String.raw`
import fs from 'node:fs';
import fsp from 'node:fs/promises';
const append = fsp.appendFile;
fsp.appendFile = async(file, data, ...args) => {
 const value = await append(file, data, ...args), cwd = process.cwd();
 if(fs.existsSync(cwd+'/.hold-final-save') && String(data).includes('completed') && !fs.existsSync(cwd+'/.final-save-held')) {
  fs.writeFileSync(cwd+'/.final-save-held','');
  while(!fs.existsSync(cwd+'/.release-final-save'))await new Promise(r=>setTimeout(r,5));
 }
 return value;
};
Object.defineProperty(process.stdout, 'writableLength', {configurable:true, get() {
 const cwd=process.cwd();
 if(fs.existsSync(cwd+'/.block-reader')) {fs.writeFileSync(cwd+'/.reader-blocked','');return 9*1024*1024;}
 return 0;
}});`,
  ).replace(
    "if(text==='unread-result')await until(cwd+'/.release',abort.signal);",
    "if(text==='unread-result'){await until(cwd+'/.release',abort.signal);if(abort.signal.aborted)return;yield {type:'assistant',uuid:'backpressure-answer',message:{id:'after',content:[{type:'text',text:'after final save'}],usage}};yield {type:'result',subtype:'success',usage,result:'continued'};}",
  );
  const bridge = await startBridge({}, sdk);
  const old = await running(bridge, "unread-result");
  await steer(bridge, old, "reader-unread");
  await release(bridge, ".hold-final-save");
  await end(bridge);
  await marker(bridge, ".result-sent");
  await release(bridge, ".block-reader");
  await release(bridge, ".release");
  await marker(bridge, ".reader-blocked");
  await marker(bridge, ".final-save-held");
  await fs.rm(path.join(bridge.root, ".block-reader"));
  await bridge.call("thread/loaded/list", {});
  await bridge.call("thread/loaded/list", {});
  const premature = bridge.notes.find(
    (n) => n.params?.item?.text === "after final save",
  );
  await release(bridge, ".release-final-save");
  await completed(bridge, old);
  assert.equal(premature, undefined);
  const answer = await bridge.note(
    (n) => n.params?.item?.text === "after final save",
  );
  assert.notEqual(answer.params.turnId, old);
  await completed(bridge, answer.params.turnId);
});

test("a throwing CLI close still completes the turn and preserves unknown input", async () => {
  const sdk = FAKE_SDK.replace(
    "close(){abort.abort();}",
    "close(){if(fs.existsSync(cwd+'/.throw-close')&&!fs.existsSync(cwd+'/.close-thrown')){fs.writeFileSync(cwd+'/.close-thrown','');throw Error('injected close failure');}abort.abort();}",
  );
  const bridge = await startBridge({}, sdk);
  const turn = await running(bridge, "success-late");
  await steer(bridge, turn, "close-failure");
  await read(bridge, "steer text");
  await release(bridge, ".throw-close");
  await end(bridge);
  const done = await completed(bridge, turn);
  assert.ok(existsSync(path.join(bridge.root, ".close-thrown")));
  assert.equal(done.params.turn.status, "failed");
  assert.match(done.params.turn.error.message, /injected close failure/);
  assert.equal(done.params.turn.unappliedSteers, undefined);
  assert.equal(
    (await turnItems(bridge, turn)).find((i) => i.id === "close-failure")
      .deliveryStatus,
    "submitted",
  );
  assert.deepEqual((await bridge.call("thread/loaded/list", {})).data, []);
  assert.deepEqual(await inputs(bridge), ["success-late", "steer text"]);
  assert.equal(
    bridge.notes.filter((n) => n.method === "turn/completed").length,
    1,
  );
});

test("bridge request and notification guards are disposed after settlement", async () => {
  const bridge = await startBridge();
  vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
  try {
    await bridge.call("thread/loaded/list", { limit: 100 });
    await assert.rejects(bridge.call("not/a/method", {}));
    await bridge.restart();
    assert.equal(vi.getTimerCount(), 0);
    const missing = assert.rejects(
      bridge.note(() => false),
      /notification missing/,
    );
    assert.equal(vi.getTimerCount(), 1);
    await vi.advanceTimersByTimeAsync(HANG_GUARD_MS);
    await missing;
    assert.equal(vi.getTimerCount(), 0);
  } finally {
    vi.useRealTimers();
  }
});
