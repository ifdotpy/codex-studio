// A long streamed answer through the real bridge with a scripted SDK. No model
// calls. The turn's input text is the number of text deltas per block.
import { afterEach, test } from "vitest";
import assert from "node:assert/strict";
import path from "node:path";
import { createSessionStore } from "./session-store.mjs";
import { startBridge } from "./testing/bridge-harness.mjs";

const FAKE_SDK = String.raw`
export const tool=(name,description,schema,call)=>({name,description,schema,call});
export const createSdkMcpServer=value=>({...value});
export const forkSession=async()=>({sessionId:'22222222-2222-4222-8222-222222222222'});
export const getSessionMessages=async()=>[];
const usage={input_tokens:10,output_tokens:2};
export function query({prompt}){
 const abort=new AbortController();
 return {
  mcpServerStatus:async()=>[],reconnectMcpServer:async()=>{},
  supportedModels:async()=>[{value:'default',displayName:'Default',resolvedModel:'claude-opus-5-5'}],
  accountInfo:async()=>({email:'test@example.test',subscriptionType:'Claude Max',apiProvider:'firstParty'}),
  initializationResult:async()=>({commands:[]}),
  setModel:async()=>{},setPermissionMode:async()=>{},applyFlagSettings:async()=>{},stopTask:async()=>{},
  setMcpServers:async()=>({added:[],removed:[],errors:{}}),
  close(){abort.abort();},interrupt:async()=>abort.abort(),
  async *[Symbol.asyncIterator](){
   const input=await prompt.next();
   if(input.done)return;
   const count=Number(input.value.message.content[0].text);
   yield {type:'user',isReplay:true,uuid:input.value.uuid};
   yield {type:'system',subtype:'init'};
   yield {type:'stream_event',event:{type:'message_start',message:{id:'message-1'}}};
   const blocks=['',''];
   for(const index of [0,1])
    for(let i=0;i<count;i++){
     const text='b'+index+'w'+i+' ';
     blocks[index]+=text;
     yield {type:'stream_event',event:{type:'content_block_delta',index,delta:{type:'text_delta',text}}};
    }
   yield {type:'assistant',uuid:'a-1',message:{id:'message-1',model:'claude-opus-5-5',usage,content:blocks.map(text=>({type:'text',text}))}};
   yield {type:'result',subtype:'success',usage,result:'done'};
   await new Promise(resolve=>abort.signal.addEventListener('abort',resolve,{once:true}));
  }
 };
}
`;

const bridges = [];
afterEach(async () => {
  for (const bridge of bridges.splice(0)) await bridge.close();
});

test("a long streamed answer keeps every delta, line and the saved text", async () => {
  const deltasPerBlock = 2_000;
  const bridge = await startBridge(FAKE_SDK, "claude-streaming-");
  bridges.push(bridge);
  const turn = (
    await bridge.call("turn/start", {
      threadId: bridge.thread,
      clientUserMessageId: "long-1",
      input: [{ type: "text", text: String(deltasPerBlock) }],
    })
  ).turn.id;
  const completed = await bridge.note((n) => n.method === "turn/completed");
  assert.equal(completed.params.turn.id, turn);
  assert.equal(completed.params.turn.status, "completed");
  const blocks = [0, 1].map((index) =>
    Array.from(
      { length: deltasPerBlock },
      (_, i) => "b" + index + "w" + i + " ",
    ).join(""),
  );
  const expected = blocks.join("\n");
  const deltas = bridge.notes.filter(
    (n) =>
      n.method === "item/agentMessage/delta" && n.params.itemId === "message-1",
  );
  assert.equal(deltas.length, 2 * deltasPerBlock, "one line per delta");
  assert.equal(deltas.map((n) => n.params.delta).join(""), expected);
  const answer = bridge.notes.find(
    (n) => n.method === "item/completed" && n.params.item.id === "message-1",
  );
  assert.equal(answer.params.item.text, expected);
  const saved = await createSessionStore(
    path.join(bridge.root, "state"),
  ).readAll(bridge.thread);
  const savedTurn = saved.session.turns.find((item) => item.id === turn);
  const savedAnswer = savedTurn.items.filter((item) => item.id === "message-1");
  assert.equal(savedAnswer.length, 1);
  assert.equal(savedAnswer[0].text, expected);
});
