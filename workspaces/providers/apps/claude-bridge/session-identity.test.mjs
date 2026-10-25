import { expect, test } from "vitest";
import { startBridge } from "./testing/bridge-harness.mjs";
import fs from "node:fs/promises";
import path from "node:path";
import os from "node:os";

const NEW = "22222222-2222-4222-8222-222222222222";
const CONVERSATION = "33333333-3333-4333-8333-333333333333";
const fake = `
import fs from 'node:fs';
export const tool=(name,description,schema,call)=>({name,description,schema,call});
export const createSdkMcpServer=v=>v;
export const forkSession=async()=>({sessionId:'unused'});
export const getSessionMessages=async()=>[];
export function query({prompt,options}) {
  const abort=new AbortController();
  fs.appendFileSync(options.cwd+'/.queries',JSON.stringify({resume:options.resume,sessionId:options.sessionId})+'\\n');
  return {
    accountInfo:async()=>({email:'test@example.test',subscriptionType:'Claude Max',apiProvider:'firstParty'}),
    supportedModels:async()=>[], initializationResult:async()=>({commands:[]}),
    close(){abort.abort();},
    async *[Symbol.asyncIterator]() {
      const input=await prompt.next();if(input.done)return;
      yield {type:'system',subtype:'conversation_reset',new_conversation_id:'${CONVERSATION}'};
      if(process.env.IDENTITY_SOURCE!=='init') {if(process.env.IDENTITY_SOURCE==='hook')yield {type:'system',subtype:'init',session_id:options.resume||options.sessionId};}
      if(process.env.IDENTITY_SOURCE!=='init'&&options.hooks?.SessionStart)for(const group of options.hooks.SessionStart)for(const hook of group.hooks)await hook({hook_event_name:'SessionStart',source:process.env.IDENTITY_SOURCE==='fork'?'fork':'clear',session_id:'${NEW}'},null,{signal:abort.signal});
      if(process.env.IDENTITY_SOURCE!=='hook'&&process.env.IDENTITY_SOURCE!=='fork')yield {type:'system',subtype:'init',session_id:'${NEW}',claude_code_version:'fixture'};
      yield {type:'result',subtype:'success',result:'cleared',usage:{input_tokens:0,output_tokens:0}};
      await new Promise(r=>{if(abort.signal.aborted)r();else abort.signal.addEventListener('abort',r,{once:true});});
    }
  };
}`;
for (const source of ["init", "hook", "fork", "both"])
  test(`clear/fork ${source} persists the authoritative native id and resumes it after restart`, async () => {
    const base = path.join(
      os.homedir(),
      ".cache/codex-studio/pivot1-v5-history",
    );
    await fs.mkdir(base, { recursive: true });
    const bridge = await startBridge(fake, "clear-", {
      temporaryBase: base,
      env: { IDENTITY_SOURCE: source },
    });
    try {
      const first = await bridge.call("turn/start", {
        threadId: bridge.thread,
        input: [{ type: "text", text: "/clear" }],
      });
      await bridge.note(
        (n) =>
          n.method === "turn/completed" && n.params.turn.id === first.turn.id,
      );
      expect(bridge.thread).not.toBe(NEW);
      expect(
        (await bridge.call("claude/state", { threadId: bridge.thread }))
          .nativeId,
      ).toBe(NEW);
      await bridge.restart();
      expect(
        (await bridge.call("claude/state", { threadId: bridge.thread }))
          .nativeId,
      ).toBe(NEW);
      await bridge.call("turn/start", {
        threadId: bridge.thread,
        input: [{ type: "text", text: "after clear" }],
      });
      const queries = (
        await fs.readFile(path.join(bridge.root, ".queries"), "utf8")
      )
        .trim()
        .split("\n")
        .map(JSON.parse);
      expect(queries[1].resume).toBe(NEW);
      expect(queries[1].resume).not.toBe(CONVERSATION);
    } finally {
      await bridge.close();
    }
  });
