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
// anything else: never read further input until `.end` exists.
const FAKE_SDK = String.raw`
import fs from 'node:fs';
export const tool=(name,description,schema,call)=>({name,description,schema,call});
export const createSdkMcpServer=value=>({...value});
export const forkSession=async()=>({sessionId:'22222222-2222-4222-8222-222222222222'});
export const getSessionMessages=async()=>[];
const until=async(file,signal)=>{while(!fs.existsSync(file)&&!signal.aborted)await new Promise(r=>setTimeout(r,5));};
const usage={input_tokens:10,output_tokens:2};
export function query({prompt,options}){
 const abort=new AbortController(),cwd=options.cwd;
 const read=async()=>{const input=await prompt.next();if(!input.done){fs.appendFileSync(cwd+'/.inputs',input.value.message.content[0].text+'\n');fs.appendFileSync(cwd+'/.sdk-inputs',JSON.stringify(input.value)+'\n');}return input;};
 return {
  mcpServerStatus:async()=>[],reconnectMcpServer:async()=>{},
  supportedModels:async()=>[{value:'default',displayName:'Default',resolvedModel:'claude-opus-5-5'}],
  accountInfo:async()=>({email:'test@example.test',subscriptionType:'Claude Max',apiProvider:'firstParty'}),
  initializationResult:async()=>({commands:[]}),
  setModel:async()=>{},setPermissionMode:async()=>{},applyFlagSettings:async()=>{},stopTask:async()=>{},
  setMcpServers:async()=>({added:[],removed:[],errors:{}}),
  close(){abort.abort();},interrupt:async()=>abort.abort(),
  async *[Symbol.asyncIterator](){
   while(!abort.signal.aborted){
    const input=await read();
    if(input.done)return;
    const text=input.value.message.content[0].text;
    yield {type:'user',isReplay:true,uuid:input.value.uuid};
    if(input.value.shouldQuery===false){const fail=text==='prefix-fail';yield {type:'result',subtype:fail?'error_during_execution':'success',is_error:fail,errors:fail?['prefix failed']:undefined,num_turns:0,user_message_uuid:input.value.uuid,usage};continue;}
    yield {type:'system',subtype:'init'};
    yield {type:'stream_event',event:{type:'message_start',message:{id:'message-'+text}}};
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

async function startBridge() {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), "claude-steer-"));
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
      if (message.error) throw new Error(message.error.message);
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
