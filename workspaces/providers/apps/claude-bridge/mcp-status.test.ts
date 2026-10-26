import { test } from "vitest";
import assert from "node:assert/strict";
import { startBridge } from "./testing/bridge-harness.ts";

const FAKE_SDK = String.raw`
import fs from 'node:fs';
export const tool=(name,description,schema,call)=>({name,description,schema,call});
export const createSdkMcpServer=value=>({...value});
export const forkSession=async()=>({sessionId:'22222222-2222-4222-8222-222222222222'});
export const getSessionMessages=async()=>[];
const usage={input_tokens:10,output_tokens:2};
export function query({prompt,options}){
 let reconnected=false;const abort=new AbortController(),log=(event)=>fs.appendFileSync(options.cwd+'/.log',JSON.stringify(event)+'\n');
 log({query:{resume:options.resume??null,sessionId:options.sessionId??null}});
 return {
  mcpServerStatus:async()=>[
   {name:'one',status:'connected',scope:'project',tools:[{name:'read'}],config:{env:{TOKEN:'private'}}},
   {name:'two',status:reconnected?'connected':'failed',error:reconnected?undefined:'offline'}
  ],reconnectMcpServer:async(name)=>{if(name!=='two')throw new Error('unknown server');reconnected=true;log({reconnect:name});},
  supportedModels:async()=>[{value:'default',displayName:'Default',resolvedModel:'claude-opus-5-5'}],
  accountInfo:async()=>({email:'test@example.test',subscriptionType:'Claude Max',apiProvider:'firstParty'}),
  initializationResult:async()=>({commands:[]}),
  setModel:async()=>{},setPermissionMode:async()=>{},applyFlagSettings:async()=>{},stopTask:async()=>{},
  setMcpServers:async()=>({added:[],removed:[],errors:{}}),
  close(){if(!abort.signal.aborted)log({close:true});abort.abort();},interrupt:async()=>abort.abort(),
  async *[Symbol.asyncIterator](){
   while(!abort.signal.aborted){
    const input=await prompt.next();
    if(input.done)return;
    log({input:input.value.message.content[0].text});
    yield {type:'user',isReplay:true,uuid:input.value.uuid};
    yield {type:'system',subtype:'init'};
    yield {type:'result',subtype:'success',usage,result:'done'};
   }
  }
 };
}
`;

test("MCP status and reconnect use the session query and preserve discovery pagination", async () => {
  const bridge = await startBridge(FAKE_SDK);
  try {
    await assert.rejects(
      bridge.call("mcpServerStatus/list", { threadId: bridge.thread }),
      /query is not loaded/,
    );
    const started = await bridge.call("turn/start", {
      threadId: bridge.thread,
      input: [{ type: "text", text: "start" }],
    });
    await bridge.note(
      (n) =>
        n.method === "turn/completed" && n.params.turn.id === started.turn.id,
    );
    const first = await bridge.call("mcpServerStatus/list", {
      threadId: bridge.thread,
      limit: 1,
    });
    assert.deepEqual(first, {
      data: [
        {
          name: "one",
          status: "connected",
          scope: "project",
          tools: [{ name: "read" }],
        },
      ],
      nextCursor: "1",
    });
    const second = await bridge.call("mcpServerStatus/list", {
      threadId: bridge.thread,
      cursor: first.nextCursor,
      limit: 1,
    });
    assert.equal(second.data[0].status, "failed");
    assert.equal(second.nextCursor, null);
    await bridge.call("mcpServer/reconnect", {
      threadId: bridge.thread,
      name: "two",
    });
    assert.equal(
      (await bridge.call("mcpServerStatus/list", { threadId: bridge.thread }))
        .data[1].status,
      "connected",
    );
    await assert.rejects(
      bridge.call("mcpServer/reconnect", {
        threadId: bridge.thread,
        name: "missing",
      }),
      /unknown server/,
    );
    await assert.rejects(
      bridge.call("mcpServerStatus/list", {
        threadId: bridge.thread,
        cursor: "wrong",
      }),
      /cursor/,
    );
    await bridge.call("thread/unsubscribe", { threadId: bridge.thread });
    await assert.rejects(
      bridge.call("mcpServerStatus/list", { threadId: bridge.thread }),
      /query is not loaded/,
    );
  } finally {
    await bridge.close();
  }
});
