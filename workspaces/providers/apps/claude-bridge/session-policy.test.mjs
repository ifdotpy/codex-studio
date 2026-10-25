import { afterEach, test } from "vitest";
import assert from "node:assert/strict";
import fs from "node:fs/promises";
import path from "node:path";
import { startBridge } from "./testing/bridge-harness.mjs";

const SDK = String.raw`
import fs from 'node:fs';
export const tool=(name,description,schema,call)=>({name,description,schema,call});
export const createSdkMcpServer=value=>value;
export const forkSession=async()=>({sessionId:'22222222-2222-4222-8222-222222222222'});
export const getSessionMessages=async()=>[];
export const resolveSettings=async()=>({effective:JSON.parse(process.env.STUDIO_FAKE_SETTINGS||'{}')});
export function query({prompt,options}){
 const abort=new AbortController();
 return {
  mcpServerStatus:async()=>[],
  supportedModels:async()=>[], initializationResult:async()=>({commands:[]}),
  accountInfo:async()=>({email:'test@example.test',subscriptionType:'Claude Max',apiProvider:'firstParty'}),
  close(){abort.abort();},interrupt:async()=>{fs.writeFileSync(options.cwd+'/.interrupted','yes');abort.abort();},
  async *[Symbol.asyncIterator](){
   const input=await prompt.next();if(input.done)return;
   fs.writeFileSync(options.cwd+'/options.json',JSON.stringify(options));
   yield {type:'user',isReplay:true,uuid:input.value.uuid};
   yield {type:'assistant',message:{id:'call',usage:{input_tokens:1,output_tokens:1,cache_creation_input_tokens:100,cache_creation:{ephemeral_1h_input_tokens:100}},content:[{type:'tool_use',id:'mcp-call',name:'mcp__remote__slow',input:{}}]}};
   if(options.env.CLAUDE_AUTO_BACKGROUND_TASKS!=='true'){
    await waitForFile(options.cwd+'/.finish',{signal:abort.signal}).catch(()=>{});return;
   }
   await new Promise(resolve=>setTimeout(resolve,Number(options.env.CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS)));
   yield {type:'system',subtype:'task_started',task_id:'mcp-task',task_type:'mcp_task',tool_use_id:'mcp-call',is_backgrounded:true,description:'Slow remote tool'};
   yield {type:'system',subtype:'background_tasks_changed',tasks:[{task_id:'mcp-task',task_type:'mcp_task',description:'Slow remote tool'}]};
   yield {type:'user',message:{content:[{type:'tool_result',tool_use_id:'mcp-call',content:'Task continues in the background'}]}};
   yield {type:'result',subtype:'success',result:'Waiting in the background'};
   await waitForFile(options.cwd+'/.finish',{signal:abort.signal}).catch(()=>{});
   if(abort.signal.aborted)return;
   yield {type:'system',subtype:'task_notification',task_id:'mcp-task',status:'completed',summary:'Remote tool finished'};
   yield {type:'system',subtype:'background_tasks_changed',tasks:[]};
   yield {type:'assistant',message:{id:'wake',content:[{type:'text',text:'Remote tool finished'}]}};
   yield {type:'result',subtype:'success',result:'Remote tool finished'};
   await new Promise(resolve=>abort.signal.addEventListener('abort',resolve,{once:true}));
  }
 };
}
`;
const bridges = [];
afterEach(async () => {
  for (const bridge of bridges.splice(0)) await bridge.close();
});
async function start(options = {}) {
  const bridge = await startBridge(SDK, "claude-session-policy-", {
    clock: true,
    ...options,
  });
  bridges.push(bridge);
  return bridge;
}
async function begin(bridge) {
  const { turn } = await bridge.call("turn/start", {
    threadId: bridge.thread,
    input: [{ type: "text", text: "Start the slow tool" }],
  });
  await bridge.note(
    (n) => n.method === "item/started" && n.params.item.id === "mcp-call",
  );
  return turn.id;
}

test("MCP timer releases the turn, retains the task, avoids a false stall, and wakes once", async () => {
  const bridge = await start({
    env: { STUDIO_CLAUDE_TOOL_SILENCE_MS: "180000" },
  });
  const turn = await begin(bridge);
  const options = JSON.parse(
    await fs.readFile(path.join(bridge.root, "options.json"), "utf8"),
  );
  assert.equal(options.env.CLAUDE_AUTO_BACKGROUND_TASKS, "true");
  assert.equal(options.env.CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS, "120000");
  await bridge.advanceTime(119999);
  assert.equal(
    bridge.notes.some((n) => n.method === "turn/completed"),
    false,
  );
  await bridge.advanceTime(1);
  await bridge.advanceUntil(
    (n) => n.method === "turn/completed" && n.params.turn.id === turn,
  );
  const diagnostics = await bridge.call("claude/diagnostics", {});
  assert.equal(diagnostics.activeTurns, 0);
  assert.equal(diagnostics.backgroundQueries, 1);
  assert.equal(
    (await bridge.call("claude/state", { threadId: bridge.thread })).tasks[0]
      .task_id,
    "mcp-task",
  );
  await bridge.advanceTime(180001);
  assert.equal(
    bridge.notes.some((n) => n.params?.item?.stalled),
    false,
  );
  await assert.rejects(
    fs.stat(path.join(bridge.root, ".interrupted")),
    /ENOENT/,
  );
  await assert.rejects(
    bridge.call("thread/unsubscribe", { threadId: bridge.thread }),
    /Pause/,
  );
  await bridge.writeSignal(".finish");
  await bridge.advanceUntil(
    (n) => n.method === "turn/completed" && n.params.turn.id !== turn,
  );
  assert.equal(
    bridge.notes.filter((n) => n.method === "turn/started").length,
    2,
  );
  const tasks = bridge.notes.filter(
    (n) => n.params?.item?.id === "task:mcp-task",
  );
  assert.equal(tasks.at(-1).params.item.status, "completed");
  assert.equal(tasks.at(-1).params.turnId, turn);
  assert.equal(
    (await bridge.call("claude/state", { threadId: bridge.thread })).tasks
      .length,
    0,
  );
});

test.each([
  [{}, {}, "5m"],
  [{ promptCacheTtl: "1h", subagentPromptCacheTtl: "5m" }, {}, "5m"],
  [{ promptCacheTtl: "5m", subagentPromptCacheTtl: "1h" }, {}, "1h"],
  [
    { subagentPromptCacheTtl: "1h" },
    {
      CLAUDE_CODE_SUBAGENT_PROMPT_CACHE_TTL: "5m",
      CLAUDE_CODE_PROMPT_CACHE_TTL: "1h",
    },
    "5m",
  ],
  [{ subagentPromptCacheTtl: "1h" }, { FORCE_PROMPT_CACHING_5M: "1" }, "5m"],
  [{}, { ENABLE_PROMPT_CACHING_1H: "1" }, "1h"],
])(
  "a managed worker applies native subagent TTL policy (%j, %j)",
  async (settings, env, ttl) => {
    const bridge = await start({
      thread: { managedWorker: true },
      env: { ...env, STUDIO_FAKE_SETTINGS: JSON.stringify(settings) },
    });
    await begin(bridge);
    const options = JSON.parse(
      await fs.readFile(path.join(bridge.root, "options.json"), "utf8"),
    );
    assert.equal(options.env.CLAUDE_CODE_PROMPT_CACHE_TTL, ttl);
    assert.equal(
      (await bridge.call("claude/state", { threadId: bridge.thread }))
        .effectivePromptCacheTtl,
      ttl,
    );
    const report = await bridge.note(
      (n) => n.method === "thread/idleCompaction/updated",
    );
    assert.equal(report.params.report.ttl, ttl);
  },
);

test("the explicit opt-out survives restart and the foreground stall watch stays active", async () => {
  const bridge = await start({
    env: {
      STUDIO_CLAUDE_TOOL_SILENCE_MS: "180000",
      STUDIO_CLAUDE_STALL_EXIT_WAIT_MS: "10",
    },
  });
  await bridge.call("claude/settings", {
    threadId: bridge.thread,
    settings: { mcpAutoBackground: false },
  });
  await bridge.restart();
  const turn = await begin(bridge);
  const options = JSON.parse(
    await fs.readFile(path.join(bridge.root, "options.json"), "utf8"),
  );
  assert.equal(options.env.CLAUDE_AUTO_BACKGROUND_TASKS, "false");
  assert.equal(options.env.CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS, "0");
  await bridge.advanceTime(180001);
  const completed = await bridge.advanceUntil(
    (n) => n.method === "turn/completed" && n.params.turn.id === turn,
  );
  assert.equal(completed.params.turn.status, "interrupted");
  assert.equal(
    bridge.notes.some((n) => n.params?.item?.stalled === true),
    true,
  );
});

test("a lead keeps its main cache policy", async () => {
  const bridge = await start({
    env: {
      CLAUDE_CODE_PROMPT_CACHE_TTL: "1h",
      CLAUDE_CODE_SUBAGENT_PROMPT_CACHE_TTL: "5m",
    },
  });
  await begin(bridge);
  const options = JSON.parse(
    await fs.readFile(path.join(bridge.root, "options.json"), "utf8"),
  );
  assert.equal(options.env.CLAUDE_CODE_PROMPT_CACHE_TTL, "1h");
});
