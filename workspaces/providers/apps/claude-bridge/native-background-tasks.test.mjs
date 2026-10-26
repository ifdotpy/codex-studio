import { afterEach, test } from "vitest";
import assert from "node:assert/strict";
import fs from "node:fs/promises";
import path from "node:path";
import { startBridge } from "./testing/bridge-harness.mjs";

// Exercise the real bridge with task events and a native wake after turn end.
const FAKE_SDK = String.raw`
import fs from 'node:fs';
import path from 'node:path';
export const tool=(name,description,schema,call)=>({name,description,schema,call});
export const createSdkMcpServer=value=>({...value});
export const forkSession=async()=>({sessionId:'22222222-2222-4222-8222-222222222222'});
export const getSessionMessages=async()=>[];
const usage={input_tokens:10,output_tokens:2};
export function query({prompt,options}){
 const abort=new AbortController(),cwd=options.cwd;
 return {
  mcpServerStatus:async()=>[],reconnectMcpServer:async()=>{},
  supportedModels:async()=>[{value:'default',displayName:'Default',resolvedModel:'claude-opus-5-5'}],
  accountInfo:async()=>({email:'test@example.test',subscriptionType:'Claude Max',apiProvider:'firstParty'}),
  initializationResult:async()=>({commands:[]}),
  setModel:async()=>{},setPermissionMode:async()=>{},applyFlagSettings:async()=>{},
  setMcpServers:async()=>({added:[],removed:[],errors:{}}),
  close(){abort.abort();},interrupt:async()=>abort.abort(),
  async *[Symbol.asyncIterator](){
   const input=await prompt.next();
   if(input.done)return;
   yield {type:'user',isReplay:true,uuid:input.value.uuid};
   yield {type:'system',subtype:'init'};
   yield {type:'system',subtype:'task_started',task_id:'native-1',task_type:'local_bash',is_backgrounded:true,description:'Watch the build',uuid:'s-1'};
   yield {type:'system',subtype:'task_progress',task_id:'native-1',description:'Watch the build',summary:'Build still running',uuid:'s-2'};
   yield {type:'system',subtype:'task_updated',task_id:'native-1',patch:{description:'Build almost done',status:'running'},uuid:'s-3'};
   yield {type:'result',subtype:'success',usage,result:'Task started'};
   await waitForFile(cwd+'/.finish',{signal:abort.signal}).catch(()=>{});
   if(abort.signal.aborted)return;
   const status=fs.readFileSync(cwd+'/.finish','utf8');
   // A state patch can precede the notification that supplies the final output.
   yield {type:'system',subtype:'task_updated',task_id:'native-1',patch:{status:status==='stopped'?'killed':status},uuid:'s-4'};
   yield {type:'system',subtype:'task_notification',task_id:'native-1',status,output_file:path.join(cwd,'native-1.output'),summary:'Task '+status,uuid:'s-5'};
   yield {type:'assistant',uuid:'wake-message',message:{id:'wake-message',model:'claude-opus-5-5',usage,content:[{type:'text',text:'Native task finished'}]}};
   yield {type:'result',subtype:'success',usage,result:'Native task finished'};
   await new Promise(resolve=>abort.signal.addEventListener('abort',resolve,{once:true}));
  }
 };
}
`;

const bridges = [];
afterEach(async () => {
  for (const bridge of bridges.splice(0)) await bridge.close();
});

test.each([
  ["completed", "completed"],
  ["failed", "failed"],
  ["killed", "failed"],
  ["stopped", "failed"],
])(
  "a native task stays running until its %s notification and preserves the wake turn",
  async (status, expectedStatus) => {
    const bridge = await startBridge(FAKE_SDK, "claude-native-task-");
    bridges.push(bridge);
    const turn = (
      await bridge.call("turn/start", {
        threadId: bridge.thread,
        input: [{ type: "text", text: "Start a background task" }],
      })
    ).turn.id;
    await bridge.note(
      (n) => n.method === "turn/completed" && n.params.turn.id === turn,
    );
    const taskEvents = () =>
      bridge.notes.filter((n) => n.params?.item?.id === "task:native-1");
    assert.deepEqual(
      taskEvents().map((n) => [n.method, n.params.item.status]),
      Array(3).fill(["item/started", "inProgress"]),
    );
    assert.equal(
      taskEvents()[1].params.item.result.content[0].text,
      "Build still running",
    );
    assert.equal(
      taskEvents()[2].params.item.arguments.description,
      "Build almost done",
    );
    // Publish the marker only after its status is complete.
    const pending = path.join(bridge.root, ".finish-pending");
    await fs.writeFile(pending, status);
    await fs.rename(pending, path.join(bridge.root, ".finish"));
    const wake = await bridge.note(
      (n) => n.method === "turn/completed" && n.params.turn.id !== turn,
    );
    assert.equal(wake.params.turn.status, "completed");
    const events = taskEvents();
    assert.deepEqual(
      events.map((n) => [n.method, n.params.item.status]),
      [
        ...Array(4).fill(["item/started", "inProgress"]),
        ["item/completed", expectedStatus],
      ],
    );
    const terminal = events.at(-1);
    assert.equal(terminal.params.turnId, turn);
    assert.equal(terminal.params.item.tool, "local_bash");
    assert.equal(
      terminal.params.item.outputFile,
      path.join(bridge.root, "native-1.output"),
    );
    assert.equal(terminal.params.item.result.content[0].text, "Task " + status);
    assert.equal(
      bridge.notes.filter((n) => n.method === "turn/started").length,
      2,
    );
  },
);
