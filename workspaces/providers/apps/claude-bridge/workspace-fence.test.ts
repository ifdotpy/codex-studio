// Exercise thread start/resume through the real bridge with an SDK fixture.
import { afterEach, test } from "vitest";
import assert from "node:assert/strict";
import fs from "node:fs/promises";
import path from "node:path";
import { startBridge } from "./testing/bridge-harness.ts";

const FAKE_SDK = String.raw`
import {writeFileSync} from 'node:fs';
import path from 'node:path';
export const tool=(name,description,schema,call)=>({name,description,schema,call});
export const createSdkMcpServer=value=>({...value});
export const forkSession=async()=>({sessionId:'fork'});
export const getSessionMessages=async()=>[];
export function query({prompt,options}){
 writeFileSync(path.join(options.cwd,'sdk-env.json'),JSON.stringify({
  tree:options.env.CLAUDE_CODE_HOST_WORKTREE,
  fence:options.env.CLAUDE_CODE_HOST_WORKTREE_FENCE,
  permissionMode:options.permissionMode,
  account:options.env.STUDIO_CLAUDE_ACCOUNT,
 }));
 const abort=new AbortController();
 return {
  mcpServerStatus:async()=>[],reconnectMcpServer:async()=>{},
  supportedModels:async()=>[],
  accountInfo:async()=>({email:'test@example.test',subscriptionType:'Claude Max',apiProvider:'firstParty'}),
  initializationResult:async()=>({commands:[]}),
  setModel:async()=>{},setPermissionMode:async()=>{},applyFlagSettings:async()=>{},stopTask:async()=>{},
  setMcpServers:async()=>({added:[],removed:[],errors:{}}),
  close(){abort.abort();},interrupt:async()=>abort.abort(),
  async *[Symbol.asyncIterator](){
   const input=await prompt.next();
   if(input.done)return;
   yield {type:'user',isReplay:true,uuid:input.value.uuid};
   yield {type:'result',subtype:'success',usage:{input_tokens:1,output_tokens:1},result:'done'};
   await new Promise(resolve=>abort.signal.addEventListener('abort',resolve,{once:true}));
  }
 };
}
`;
const bridges = [];
afterEach(async () => {
  for (const bridge of bridges.splice(0)) await bridge.close();
});

test("the SDK receives each thread's host worktree fence, including resume", async () => {
  const bridge = await startBridge(FAKE_SDK, "claude-workspace-fence-");
  bridges.push(bridge);
  const tree = bridge.root;
  const fence = JSON.stringify(["/source checkout", '/sibling:quoted"path']);
  const options = {
    cwd: tree,
    dynamicTools: [],
    claude: {
      permissionMode: "bypassPermissions",
      env: {
        CLAUDE_CODE_HOST_WORKTREE: tree,
        CLAUDE_CODE_HOST_WORKTREE_FENCE: fence,
      },
    },
  };
  const isolated = (await bridge.call("thread/start", options)).thread.id;
  let input = 0;
  async function run(threadId) {
    const turn = (
      await bridge.call("turn/start", {
        threadId,
        clientUserMessageId: `input-${++input}`,
        input: [{ type: "text", text: "check environment" }],
      })
    ).turn.id;
    const note = await bridge.note(
      (n) => n.method === "turn/completed" && n.params.turn.id === turn,
    );
    assert.equal(note.params.turn.status, "completed");
    return JSON.parse(
      await fs.readFile(path.join(tree, "sdk-env.json"), "utf8"),
    );
  }
  const expected = {
    tree,
    fence,
    permissionMode: "bypassPermissions",
    account: "test@example.test",
  };
  assert.deepEqual(await run(isolated), expected);
  const shared = await run(bridge.thread);
  assert.equal(shared.tree, undefined);
  assert.equal(shared.fence, undefined);
  assert.equal(shared.account, "test@example.test");
  await bridge.call("thread/resume", { threadId: isolated, ...options });
  assert.deepEqual(await run(isolated), expected);
});
