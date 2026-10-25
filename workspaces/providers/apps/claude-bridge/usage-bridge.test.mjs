// Real bridge protocol with SDK-declared fixtures; no model requests.
import { afterEach, expect, test } from "vitest";
import { once } from "node:events";
import fs from "node:fs/promises";
import path from "node:path";
import { waitForFile } from "./testing/file-signals.mjs";
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
import fs from "node:fs/promises";
export const tool=(name,description,schema,call)=>({name,description,schema,call});
export const createSdkMcpServer=value=>({...value});
export const forkSession=async()=>({sessionId:'22222222-2222-4222-8222-222222222222'});
export const getSessionMessages=async()=>[];
const events=${JSON.stringify(events)}, modelUsage=${JSON.stringify(modelUsage)},sdkResult=${JSON.stringify(sdkResult)},zeroResult=${JSON.stringify(zeroResult)};
export function query({prompt,options}){
 const abort=new AbortController();let delayedClose=false,stuckClose=false,reportCloseTiming=false,closeAt,restoreAppend;
 process.once('exit',()=>{if(reportCloseTiming)process.stdout.write(JSON.stringify({method:'fixture/shutdownTiming',params:{durationMs:Date.now()-closeAt}})+'\\n');});
 const result=(zero=false)=>zero?zeroResult:{...sdkResult,modelUsage:{'claude-opus-5-5':modelUsage}};
 const stream=(event,parent=null)=>({type:'stream_event',event,parent_tool_use_id:parent});
 return {mcpServerStatus:async()=>[],reconnectMcpServer:async()=>{},supportedModels:async()=>[{value:'default',displayName:'Default',resolvedModel:'claude-opus-5-5'}],accountInfo:async()=>({email:'test@example.test',subscriptionType:'Claude Max',apiProvider:'firstParty'}),initializationResult:async()=>({commands:[]}),setModel:async()=>{},setPermissionMode:async()=>{},applyFlagSettings:async()=>{},setMcpServers:async()=>({added:[],removed:[],errors:{}}),close(){restoreAppend?.();closeAt??=Date.now();if(stuckClose)return;if(delayedClose)setTimeout(()=>abort.abort(),20);else abort.abort();},interrupt:async()=>{process.stdout.write(JSON.stringify({method:'fixture/interrupt',params:{}})+'\\n');abort.abort();},
 async *[Symbol.asyncIterator](){
  while(!abort.signal.aborted){
   const input=await prompt.next();if(input.done)return;
   const text=input.value.message.content[0].text;
   yield {type:'user',isReplay:true,uuid:input.value.uuid};
   yield {type:'system',subtype:'init'};
   if(text==='shutdown-watchdog'||text==='shutdown-watchdog-inflight'){
    stuckClose=true;
    if(text==='shutdown-watchdog-inflight'){
     const append=fs.appendFile;let release;
     const released=new Promise(resolve=>release=resolve);
     restoreAppend=()=>{fs.appendFile=append;release();};
     fs.appendFile=async(...args)=>{
      if(String(args[1]).includes('stall:watch-tool')){
       process.stdout.write(JSON.stringify({method:'fixture/stallSaveBlocked',params:{}})+'\\n');
       await released;
      }
      return append(...args);
     };
    }
    yield stream(events[0]);
    yield {type:'assistant',uuid:'tool-frame',message:{id:'tool-frame',content:[{type:'tool_use',id:'watch-tool',name:'Read',input:{file_path:'x'}}]}};
    await new Promise(()=>{});
   }
   if(text==='background-stuck'){
    stuckClose=true;
    yield stream(events[0],'background-tool');
    yield result();
    await new Promise(()=>{});
   }
   if(text==='shutdown-deferred'){
    stuckClose=true;
    yield stream(events[0]);
    await waitForFile(options.cwd+'/.finish',{signal:abort.signal});
    yield result();
    await new Promise(()=>{});
   }
   if(text==='shutdown-error'||text==='shutdown-persist'){
    yield stream(events[0]);
    if(text==='shutdown-persist'){
     const append=fs.appendFile,truncate=fs.truncate;
     restoreAppend=()=>{fs.appendFile=append;fs.truncate=truncate;};
     // The journal repair is the last asynchronous step before rejection.
     // Its next check-phase callback runs after the entire rejection chain,
     // including persistLater's failure recording, has settled.
     fs.truncate=async(...args)=>{
      const value=await truncate(...args);
      setImmediate(()=>process.stdout.write(JSON.stringify({method:'fixture/persistFailureSettled',params:{}})+'\\n'));
      return value;
     };
     fs.appendFile=async(...args)=>{
      if(String(args[0]).includes('/state/sessions/')){
       throw new Error('injected persist failure');
      }
      return append(...args);
     };
     yield {type:'assistant',uuid:'persist-trigger',message:{id:'persist-trigger',content:[{type:'text',text:'trigger save'}]}};
    }
    await new Promise(resolve=>abort.signal.addEventListener('abort',resolve,{once:true}));
    if(text==='shutdown-persist')throw new Error('closed after persist failure');
    yield zeroResult;
    return;
   }
   if(text==='discard-stuck'){
    stuckClose=true;
    yield stream({...events[0],message:{...events[0].message,id:'main-response'}});
    yield stream(events[0],'stuck-parent');
    yield {type:'assistant',parent_tool_use_id:'stuck-parent',uuid:'child-started',message:{id:'child-started',content:[{type:'text',text:'child-started'}]}};
    await new Promise(()=>{});
   }
   if(text==='shutdown'||text==='shutdown-stuck'||text==='shutdown-timing'||text==='interrupt'){
    delayedClose=text==='shutdown';stuckClose=text==='shutdown-stuck'||text==='shutdown-timing';reportCloseTiming=text==='shutdown-timing';
    yield stream(events[0]);
    await new Promise(resolve=>abort.signal.addEventListener('abort',resolve,{once:true}));
    if(text==='interrupt')yield zeroResult;
    return;
   }
   if(text==='background-owner'){
    yield {type:'assistant',uuid:'tool-owner',message:{id:'owner',model:'claude-opus-5-5',content:[{type:'tool_use',id:'background-tool',name:'Task',input:{description:'background'}}]}};
    yield result();continue;
   }
   if(text==='background-next'){
    for(const event of events)yield stream(event,'background-tool');
    yield result();continue;
   }
   if(text==='windows-alias'){
    yield {...sdkResult,modelUsage:{'claude-opus-5-5':modelUsage,'claude-opus-5-5[1m]':{...modelUsage,contextWindow:1000000,canonicalModel:'claude-opus-5-5'}}};continue;
   }
   if(text==='windows'){
    yield {...sdkResult,modelUsage:{'claude-opus-5-5':modelUsage,'other-model':{...modelUsage,contextWindow:1000000}}};continue;
   }
   if(text==='background'){
    yield stream(events[0],'background-tool');
    yield result();
    await waitForFile(options.cwd+'/.finish',{signal:abort.signal});
    yield stream(events[1],'background-tool');yield stream(events[2],'background-tool');
    yield {type:'assistant',parent_tool_use_id:'background-tool',uuid:'child-done',message:{id:'response-1',content:[{type:'text',text:'background-finished'}]}};
    continue;
   }
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
async function setup(thread = {}, env = {}) {
  const b = await startBridge(sdk, "claude-usage-", { thread, env });
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
  await b.writeSignal(".finish");
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
  await b.writeSignal(".finish");
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

test("background subagent final usage survives completion of its owning Studio turn", async () => {
  const b = await setup();
  const turn = await start(b, "background");
  await done(b, turn);
  await b.writeSignal(".finish");
  await b.note(
    (n) =>
      n.method === "item/completed" &&
      n.params.item.text === "Subagent: background-finished",
  );
  expect(observations(b).filter((o) => o.kind === "response")).toMatchObject([
    {
      responseId: "response-1",
      turnId: turn,
      parentToolUseId: "background-tool",
      tokens: { output: 720 },
    },
  ]);
  expect(observations(b).filter((o) => o.kind === "gap")).toEqual([]);
});
test("response usage includes the known model context window before result", async () => {
  const b = await setup({ model: "claude-opus-5-5", contextWindow: 200000 });
  await done(b, await start(b, "result-only"));
  const turn = await start(b, "normal");
  await done(b, turn);
  expect(observations(b).find((o) => o.kind === "response")).toHaveProperty(
    "modelContextWindow",
    200000,
  );
});

test("emitted notifications match the shared Rust dispatch golden fixture", async () => {
  const golden = JSON.parse(
    await fs.readFile(
      new URL(
        "../../../runtime/packages/runtime-schema/src/fixtures/claude-usage-notifications.json",
        import.meta.url,
      ),
      "utf8",
    ),
  );
  const b = await setup({ model: "claude-opus-5-5", contextWindow: 200000 });
  await done(b, await start(b, "result-only"));
  const before = b.notes.length;
  await done(b, await start(b, "normal"));
  await done(b, await start(b, "interrupted"));
  const notes = b.notes
    .slice(before)
    .filter((n) => n.method === "thread/tokenUsage/updated");
  const selected = [
    notes.find((n) => n.params.studioUsage.kind === "response"),
    notes.find((n) => n.params.studioUsage.kind === "cumulative"),
    notes.find((n) => n.params.studioUsage.kind === "gap"),
  ];
  const normalized = selected.map((n) => ({
    ...n.params,
    threadId: "thread",
    turnId: "turn",
    studioUsage: { ...n.params.studioUsage, turnId: "turn" },
  }));
  expect(normalized).toEqual(golden);
});

test("background response starts retain the tool originating turn across Studio turns", async () => {
  const b = await setup();
  const owner = await start(b, "background-owner");
  await done(b, owner);
  const next = await start(b, "background-next");
  await done(b, next);
  const note = b.notes.find((n) => n.params?.studioUsage?.kind === "response");
  expect(note.params.studioUsage).toMatchObject({
    turnId: owner,
    parentToolUseId: "background-tool",
  });
  expect(note.params.turnId).toBe(owner);
});
test("shutdown waits for delayed SDK close and flushes one unfinished response gap", async () => {
  const b = await setup();
  const turn = await start(b, "shutdown");
  await b.note((n) => n.method === "provider/generationStarted");
  await b.close();
  expect(observations(b)).toEqual([
    {
      kind: "gap",
      responseId: "response-1",
      model: "claude-opus-5-5",
      turnId: turn,
      reason: "queryClosed",
    },
  ]);
});
test("response context windows follow their own model rather than the query maximum", async () => {
  const b = await setup();
  await done(b, await start(b, "windows"));
  await done(b, await start(b, "normal"));
  expect(observations(b).find((o) => o.kind === "response")).toHaveProperty(
    "modelContextWindow",
    200000,
  );
});
test("turn interrupt reports unfinished usage as a gap rather than zero tokens", async () => {
  const b = await setup();
  const turn = await start(b, "interrupt");
  await b.note((n) => n.method === "provider/generationStarted");
  await b.call("turn/interrupt", { threadId: b.thread, turnId: turn });
  await done(b, turn);
  expect(observations(b)).toEqual([
    {
      kind: "gap",
      responseId: "response-1",
      model: "claude-opus-5-5",
      turnId: turn,
      reason: "streamInterrupted",
    },
  ]);
});

test("shutdown bounds a stuck SDK consumer and still reports its gap once", async () => {
  const b = await setup({}, { STUDIO_CLAUDE_STALL_EXIT_WAIT_MS: "50" });
  const turn = await start(b, "shutdown-stuck");
  await b.note((n) => n.method === "provider/generationStarted");
  await b.close();
  expect(observations(b)).toEqual([
    {
      kind: "gap",
      responseId: "response-1",
      model: "claude-opus-5-5",
      turnId: turn,
      reason: "queryClosed",
    },
  ]);
});
test("an unknown response model never borrows another model context window", async () => {
  const b = await setup({ model: "other-model", contextWindow: 1000000 });
  await done(b, await start(b, "normal"));
  expect(observations(b).find((o) => o.kind === "response")).not.toHaveProperty(
    "modelContextWindow",
  );
});

test("virtual-clock harness advances the bounded shutdown of a stuck SDK", async () => {
  const b = await startBridge(sdk, "claude-usage-clock-", {
    clock: true,
    env: { STUDIO_CLAUDE_STALL_EXIT_WAIT_MS: "50" },
  });
  bridges.push(b);
  const turn = await start(b, "shutdown-stuck");
  await b.note((n) => n.method === "provider/generationStarted");
  await b.close();
  expect(observations(b)).toEqual([
    {
      kind: "gap",
      responseId: "response-1",
      model: "claude-opus-5-5",
      turnId: turn,
      reason: "queryClosed",
    },
  ]);
});

test("discarding a stuck query drains unfinished subagent usage exactly once", async () => {
  const b = await startBridge(sdk, "claude-usage-discard-", {
    clock: true,
    env: {
      STUDIO_CLAUDE_STALL_EXIT_WAIT_MS: "50",
      STUDIO_CLAUDE_STALL_INTERRUPT_WAIT_MS: "50",
    },
  });
  bridges.push(b);
  const turn = await start(b, "discard-stuck");
  await b.note(
    (n) =>
      n.method === "item/completed" &&
      n.params.item.text === "Subagent: child-started",
  );
  await b.call("turn/interrupt", { threadId: b.thread, turnId: turn });
  await b.advanceUntil(
    (n) => n.method === "turn/completed" && n.params.turn.id === turn,
  );
  expect(observations(b).filter((o) => o.responseId === "response-1")).toEqual([
    {
      kind: "gap",
      responseId: "response-1",
      model: "claude-opus-5-5",
      turnId: turn,
      reason: "queryClosed",
    },
  ]);
  await b.close();
  expect(
    observations(b).filter((o) => o.responseId === "response-1"),
  ).toHaveLength(1);
});

test("restart never seeds a response from a persisted query-wide maximum window", async () => {
  const b = await setup({ model: "claude-opus-5-5" });
  await done(b, await start(b, "windows"));
  await b.restart();
  await done(b, await start(b, "normal"));
  expect(observations(b).find((o) => o.kind === "response")).not.toHaveProperty(
    "modelContextWindow",
  );
});

test("an exact model window wins over a later pricing alias with a larger context", async () => {
  const b = await setup();
  await done(b, await start(b, "windows-alias"));
  await done(b, await start(b, "normal"));
  expect(observations(b).find((o) => o.kind === "response")).toHaveProperty(
    "modelContextWindow",
    200000,
  );
});

test("shutdown never spends the full CLI exit timeout on a stuck consumer", async () => {
  const b = await startBridge(sdk, "claude-usage-shutdown-budget-", {
    clock: true,
    env: { STUDIO_CLAUDE_STALL_EXIT_WAIT_MS: "10000" },
  });
  bridges.push(b);
  await start(b, "shutdown-timing");
  await b.note((n) => n.method === "provider/generationStarted");
  await b.close();
  const timing = await b.note((n) => n.method === "fixture/shutdownTiming");
  expect(timing.params.durationMs).toBeLessThanOrEqual(2000);
});

test("scripted Rust SDK close ends an aborted wait without inventing a successful response", async () => {
  const fixture = await fs.readFile(
    new URL(
      "../../../runtime/apps/runtime-server/tests/fixtures/claude-sdk.mjs",
      import.meta.url,
    ),
    "utf8",
  );
  const b = await startBridge(fixture, "claude-usage-rust-close-");
  bridges.push(b);
  await start(b, "wait");
  await waitForFile(path.join(b.root, ".sdk-inputs"));
  await b.close();
  expect(
    b.notes
      .filter((n) => n.method === "turn/completed")
      .some((n) => n.params.turn.status === "completed"),
  ).toBe(false);
  expect(b.notes.some((n) => n.params?.item?.text === "Visible answer")).toBe(
    false,
  );
});

test("intentional shutdown preserves an unfinished turn for reconciliation instead of publishing failure", async () => {
  const b = await setup();
  await start(b, "shutdown");
  await b.note((n) => n.method === "provider/generationStarted");
  await b.close();
  expect(b.notes.filter((n) => n.method === "turn/completed")).toEqual([]);
  expect(observations(b)).toMatchObject([
    { kind: "gap", reason: "queryClosed" },
  ]);
});

for (const scenario of [
  "shutdown-error",
  "shutdown-persist",
  "shutdown-deferred",
])
  test(
    scenario + " cannot finalize an intentionally closing turn",
    async () => {
      const b = await startBridge(sdk, "claude-usage-finalization-admission-", {
        clock: true,
      });
      bridges.push(b);
      const turn = await start(b, scenario);
      await b.note((n) => n.method === "provider/generationStarted");
      if (scenario === "shutdown-persist")
        await b.note((n) => n.method === "fixture/persistFailureSettled");
      if (scenario === "shutdown-deferred") {
        await b.call("turn/steer", {
          threadId: b.thread,
          expectedTurnId: turn,
          clientUserMessageId: "unread-steer",
          input: [{ type: "text", text: "continue" }],
        });
        await b.writeSignal(".finish");
        await b.note((n) => n.params?.studioUsage?.kind === "cumulative");
        await b.advanceTime(2000);
      }
      await b.close();
      expect(b.notes.filter((n) => n.method === "turn/completed")).toEqual([]);
      expect(
        observations(b).filter((o) => o.responseId === "response-1"),
      ).toEqual([
        {
          kind: "gap",
          responseId: "response-1",
          model: "claude-opus-5-5",
          turnId: turn,
          reason: "queryClosed",
        },
      ]);
    },
  );

test("harness close reaps a signal-killed bridge and repeats safely", async () => {
  const b = await startBridge(sdk, "claude-usage-harness-signal-", {
    clock: true,
  });
  bridges.push(b);
  const child = b.process;
  const exited = once(child, "exit");
  child.kill("SIGKILL");
  await exited;
  await b.close();
  await b.close();
});

test("harness close reaps a bridge whose virtual clock cannot be driven", async () => {
  const b = await startBridge(sdk, "claude-usage-harness-disconnect-", {
    clock: true,
  });
  bridges.push(b);
  await start(b, "shutdown-deferred");
  await b.note((n) => n.method === "provider/generationStarted");
  const child = b.process;
  child.disconnect();
  await b.close();
  expect(child.exitCode !== null || child.signalCode !== null).toBe(true);
});

test("unexpected SDK end still publishes one failed completion", async () => {
  const b = await setup();
  const turn = await start(b, "query-close");
  await done(b, turn);
  expect(b.notes.filter((n) => n.method === "turn/completed")).toMatchObject([
    {
      params: {
        turn: {
          id: turn,
          status: "failed",
          error: { message: "Claude ended without a completed turn" },
        },
      },
    },
  ]);
});

for (const method of ["thread/resume", "claude/settings", "thread/unsubscribe"])
  test(
    method +
      " drains unfinished background usage before removing a stuck query",
    async () => {
      const b = await startBridge(sdk, "claude-usage-removal-", {
        clock: true,
      });
      bridges.push(b);
      const turn = await start(b, "background-stuck");
      await done(b, turn);
      expect(observations(b).some((o) => o.kind === "gap")).toBe(false);
      await b.call(method, { threadId: b.thread, settings: {} });
      expect((await b.call("thread/loaded/list", {})).data).toEqual([]);
      const expected = [
        {
          kind: "gap",
          responseId: "response-1",
          model: "claude-opus-5-5",
          turnId: turn,
          reason: "queryClosed",
        },
      ];
      expect(
        observations(b).filter((o) => o.responseId === "response-1"),
      ).toEqual(expected);
      await b.close();
      expect(
        observations(b).filter((o) => o.responseId === "response-1"),
      ).toEqual(expected);
    },
  );

for (const scenario of ["shutdown-watchdog", "shutdown-watchdog-inflight"])
  test(
    scenario + " cannot recover a stalled tool during the shutdown drain",
    async () => {
      const b = await startBridge(sdk, "claude-shutdown-watchdog-", {
        clock: true,
        env: {
          STUDIO_CLAUDE_TOOL_SILENCE_MS: "100",
          STUDIO_CLAUDE_STALL_INTERRUPT_WAIT_MS: "100",
        },
      });
      bridges.push(b);
      const turn = await start(b, scenario);
      await b.note(
        (n) => n.method === "item/started" && n.params.item.id === "watch-tool",
      );
      if (scenario === "shutdown-watchdog-inflight") {
        await b.advanceTime(100);
        await b.note((n) => n.method === "fixture/stallSaveBlocked");
      }
      const before = b.notes.length;
      await b.close();
      expect(b.notes.some((n) => n.method === "fixture/interrupt")).toBe(false);
      expect(
        b.notes
          .slice(before)
          .filter(
            (n) =>
              n.method === "item/completed" &&
              (n.params.item.stalled || n.params.item.studioNotice),
          ),
      ).toEqual([]);
      expect(b.notes.filter((n) => n.method === "turn/completed")).toEqual([]);
      expect(
        observations(b).filter((o) => o.responseId === "response-1"),
      ).toEqual([
        {
          kind: "gap",
          responseId: "response-1",
          model: "claude-opus-5-5",
          turnId: turn,
          reason: "queryClosed",
        },
      ]);
    },
  );
