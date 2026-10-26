import { expect, test } from "vitest";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { startBridge } from "./testing/bridge-harness.mjs";

const SDK = String.raw`
import fs from 'node:fs';
import path from 'node:path';
import {randomUUID} from 'node:crypto';
export const tool=(name,description,schema,call)=>({name,description,schema,call});
export const createSdkMcpServer=v=>v;
export const forkSession=async()=>{throw new Error('unexpected fork');};
export function query({prompt,options}) {
 const abort=new AbortController(),sessionId=options.sessionId||options.resume,cwd=options.cwd;
 const file=path.join(process.env.CLAUDE_CONFIG_DIR,'projects',cwd.replace(/[^a-zA-Z0-9]/g,'-'),sessionId+'.jsonl');
 fs.mkdirSync(path.dirname(file),{recursive:true});
 let parent=null;
 const record=input=>{const row={type:'user',uuid:input.uuid,parentUuid:parent,sessionId,cwd,isSidechain:false,timestamp:new Date().toISOString(),message:input.message,...(input.isSynthetic?{isMeta:true,origin:{kind:'unclassified'}}:{})};fs.appendFileSync(file,JSON.stringify(row)+'\n');parent=row.uuid;};
 return {
 accountInfo:async()=>({email:'test@example.test',subscriptionType:'Claude Max',apiProvider:'firstParty'}),supportedModels:async()=>[],initializationResult:async()=>({commands:[]}),
 close(){abort.abort();},
 async *[Symbol.asyncIterator]() {
   const initial=await prompt.next(); if(initial.done)return;
   record(initial.value);
   yield {type:'system',subtype:'init',session_id:sessionId};
   yield {type:'user',isReplay:true,uuid:initial.value.uuid};
   process.stdout.write(JSON.stringify({method:'fixture/anchor',params:{}})+'\n');
   const parts=[];
   do {const next=await prompt.next();if(next.done)return;parts.push(next.value);}while(parts.at(-1).shouldQuery===false);
   const damage=process.env.HISTORY_DAMAGE;
   if(damage==='all')for(const part of parts)record(part);
   if(damage==='partial')record(parts[0]);
   if(damage==='malformed uuid'||damage==='unknown record')fs.appendFileSync(file,JSON.stringify({type:damage==='unknown record'?'future-input':'assistant',uuid:damage==='malformed uuid'?'malformed':randomUUID(),parentUuid:parent,sessionId,cwd,isSidechain:false,message:{role:'assistant',content:[]}})+'\n');
   if(damage==='unknown system')fs.appendFileSync(file,JSON.stringify({type:'system',subtype:'future-input',uuid:randomUUID(),parentUuid:parent,sessionId,cwd,isSidechain:false})+'\n');
   if(damage==='malformed auxiliary')fs.appendFileSync(file,JSON.stringify({type:'queue-operation',operation:42,sessionId})+'\n');
   if(damage==='invalid block')fs.appendFileSync(file,JSON.stringify({type:'user',uuid:randomUUID(),parentUuid:parent,sessionId,cwd,isSidechain:false,message:{role:'user',content:[{type:'text',text:42}]}})+'\n');
   if(damage==='torn')fs.appendFileSync(file,'{"type":');
   if(damage==='truncated')fs.writeFileSync(file,JSON.stringify({type:'user',uuid:randomUUID(),parentUuid:parent,sessionId,cwd,isSidechain:false,message:{role:'user',content:'tail without root'}})+'\n');
   if(damage==='missing')fs.rmSync(file);
   yield {type:'result',subtype:'success',usage:{input_tokens:0,output_tokens:0},result:'done'};
   await new Promise(resolve=>{if(abort.signal.aborted)resolve();else abort.signal.addEventListener('abort',resolve,{once:true});});
 }
 };
}`;
for (const serviceFirst of [true, false])
  for (const damage of [
    "absent",
    "all",
    "partial",
    "malformed uuid",
    "unknown record",
    "unknown system",
    "malformed auxiliary",
    "invalid block",
    "torn",
    "truncated",
    "missing",
  ]) {
    test(`closed composite ${serviceFirst ? "service/human" : "human/service"}: ${damage}`, async () => {
      const base = path.join(
        os.homedir(),
        ".cache/codex-studio/pivot1-v5-history",
      );
      await fs.mkdir(base, { recursive: true });
      const bridge = await startBridge(SDK, "recovery-", {
        temporaryBase: base,
        env: {
          HISTORY_DAMAGE: damage,
          STUDIO_CLAUDE_STEER_ECHO_GRACE_MS: "10",
          STUDIO_CLAUDE_STALL_EXIT_WAIT_MS: "10",
        },
      });
      try {
        const { turn } = await bridge.call("turn/start", {
          threadId: bridge.thread,
          input: [{ type: "text", text: "anchor" }],
        });
        await bridge.note((n) => n.method === "fixture/anchor");
        const input = [
          { type: "text", text: "service", inputOrigin: "studio_event" },
          { type: "text", text: "human", inputOrigin: "human" },
        ];
        if (!serviceFirst) input.reverse();
        await bridge.call("turn/steer", {
          threadId: bridge.thread,
          expectedTurnId: turn.id,
          clientUserMessageId: "exact-input",
          input,
        });
        const done = await bridge.note(
          (n) => n.method === "turn/completed" && n.params.turn.id === turn.id,
        );
        expect(done.params.turn.unappliedSteers).toEqual(
          ["absent", "unknown system"].includes(damage)
            ? ["exact-input"]
            : undefined,
        );
        const thread = (
          await bridge.call("thread/read", {
            threadId: bridge.thread,
            includeTurns: true,
          })
        ).thread;
        const item = thread.turns[0].items.find(
          (item) => item.id === "exact-input",
        );
        expect(item.nativeIds).toHaveLength(2);
        expect(item.deliveryStatus).toBe(
          damage === "all"
            ? "delivered"
            : ["absent", "unknown system"].includes(damage)
              ? "not_applied"
              : "submitted",
        );
        if (!["all", "absent", "unknown system"].includes(damage))
          expect(item.error).toMatch(/unknown/);
      } finally {
        await bridge.close();
      }
    });
  }
