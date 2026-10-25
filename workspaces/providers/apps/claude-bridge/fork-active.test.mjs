import { test } from "vitest";
import assert from "node:assert/strict";
import fs from "node:fs/promises";
import path from "node:path";
import { startBridge } from "./testing/bridge-harness.mjs";

const SDK = String.raw`
import fs from 'node:fs';
import path from 'node:path';
import {randomUUID} from 'node:crypto';
export const tool=(name,description,schema,call)=>({name,description,schema,call});
export const createSdkMcpServer=value=>({...value});
const histories=new Map();
export const getSessionMessages=async(id)=>structuredClone(histories.get(id)||[]);
export {forkSession} from '/work/ssd2/codex-studio-worktrees/codex-agents/5269c8f9-af99-5aff-8786-a7080305c924/node_modules/.pnpm/@anthropic-ai+claude-agent-sdk@0.3.308_@anthropic-ai+sdk@0.127.0_zod@4.6.7__@modelconte_1598676d9451b27b85e0eccec6565510/node_modules/@anthropic-ai/claude-agent-sdk/sdk.mjs';
export function query({prompt,options}){
 const abort=new AbortController(),id=options.resume||options.sessionId;
 const history=path.join(process.env.CLAUDE_CONFIG_DIR,'projects',options.cwd.replace(/[^a-zA-Z0-9]/g,'-'),id+'.jsonl');
 fs.mkdirSync(path.dirname(history),{recursive:true});
 let parent=fs.existsSync(history)?JSON.parse(fs.readFileSync(history,'utf8').trim().split('\n').at(-1)).uuid:null;
 const record=row=>{fs.appendFileSync(history,JSON.stringify({...row,parentUuid:parent,sessionId:id,cwd:options.cwd,isSidechain:false,timestamp:new Date().toISOString()})+'\n');parent=row.uuid;};
 const log=event=>fs.appendFileSync(options.cwd+'/.query-log',JSON.stringify({id,...event})+'\n');
 if(id)log({start:true});
 return {
  supportedModels:async()=>[{value:'default',displayName:'Default',resolvedModel:'claude-opus-5-5'}],
  accountInfo:async()=>({email:'test@example.test',subscriptionType:'Claude Max',apiProvider:'firstParty'}),
  initializationResult:async()=>({commands:[]}),
  setModel:async()=>{},setPermissionMode:async()=>{},applyFlagSettings:async()=>{},stopTask:async()=>{},
  setMcpServers:async()=>({added:[],removed:[],errors:{}}),
  close(){if(id)log({close:true});abort.abort();},interrupt:async()=>abort.abort(),
  async *[Symbol.asyncIterator](){
   while(!abort.signal.aborted){
    const input=await prompt.next(); if(input.done)return;
    const text=input.value.message.content[0].text;
    const messages=histories.get(id)||[];histories.set(id,messages);
    const user={type:'user',uuid:input.value.uuid,message:input.value.message,parent_tool_use_id:null};messages.push(user);record(user);
    yield {type:'user',isReplay:true,uuid:input.value.uuid};
    yield {type:'system',subtype:'init'};
    if(text==='running') {
      await new Promise(resolve=>abort.signal.addEventListener('abort',resolve,{once:true}));return;
    }
    const reply={type:'assistant',uuid:randomUUID(),message:{id:randomUUID(),role:'assistant',content:[{type:'text',text:'reply'}]},parent_tool_use_id:null};
    messages.push(reply);record(reply);yield reply;
    if(text==='background')yield {type:'system',subtype:'background_tasks_changed',tasks:[{task_id:'bg'}]};
    yield {type:'result',subtype:'success',usage:{input_tokens:10,output_tokens:2},result:'reply'};
   }
  }
 };
}
`;
async function start(bridge, threadId, text, completed = true) {
  const { turn } = await bridge.call("turn/start", {
    threadId,
    input: [{ type: "text", text }],
  });
  if (completed)
    await bridge.note(
      (n) => n.method === "turn/completed" && n.params.turn.id === turn.id,
    );
  return turn.id;
}
for (const mode of ["running", "background"]) {
  test(`bridge forks an earlier completed turn while source has ${mode} work`, async () => {
    const bridge = await startBridge(SDK);
    try {
      const first = await start(bridge, bridge.thread, "first");
      const latest = await start(
        bridge,
        bridge.thread,
        mode,
        mode !== "running",
      );
      const before = (
        await bridge.call("thread/read", {
          threadId: bridge.thread,
          includeTurns: true,
        })
      ).thread;
      const target = (
        await bridge.call("thread/fork", {
          threadId: bridge.thread,
          lastTurnId: first,
        })
      ).thread;
      assert.notEqual(target.id, bridge.thread);
      assert.equal(target.turns.length, 1);
      assert.equal(target.turns[0].status, "completed");
      assert.deepEqual(
        (
          await bridge.call("thread/read", {
            threadId: bridge.thread,
            includeTurns: true,
          })
        ).thread.turns,
        before.turns,
      );
      assert.deepEqual((await bridge.call("thread/loaded/list", {})).data, [
        bridge.thread,
      ]);
      for (const lastTurnId of [undefined, latest])
        await assert.rejects(
          bridge.call("thread/fork", { threadId: bridge.thread, lastTurnId }),
          /earlier completed Claude turn/,
        );
      await start(bridge, target.id, "fork input");
      assert.equal((await bridge.call("claude/activity", {})).busy, true);
      const events = (
        await fs.readFile(path.join(bridge.root, ".query-log"), "utf8")
      )
        .trim()
        .split("\n")
        .map(JSON.parse);
      assert.equal(
        events.filter((e) => e.id === bridge.thread && e.start).length,
        1,
      );
      assert.equal(
        events.some((e) => e.id === bridge.thread && e.close),
        false,
      );
      if (mode === "background")
        assert.equal(
          (
            await bridge.call("thread/backgroundTerminals/list", {
              threadId: bridge.thread,
            })
          ).data[0].task_id,
          "bg",
        );
    } finally {
      await bridge.close();
    }
  });
}
