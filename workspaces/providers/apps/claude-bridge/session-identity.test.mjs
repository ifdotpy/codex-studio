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
  process.stdout.write(JSON.stringify({method:'fixture/query',params:{resume:options.resume,sessionId:options.sessionId}})+'\\n');
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
      await bridge.note(
        (note) =>
          note.method === "fixture/query" &&
          (note.params.resume || note.params.sessionId),
      );
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

for (const phase of [
  "before",
  "after",
  "committed",
  "missing target",
  "unsolicited",
])
  test(`clear crash ${phase} transition save never resumes the old native session`, async () => {
    const crashSdk = fake
      .replace(
        "import fs from 'node:fs';",
        `
import fs from 'node:fs';
import fsp from 'node:fs/promises';
import path from 'node:path';
let fixtureCwd;
const append=fsp.appendFile;
fsp.appendFile=async(file,data,...args)=>{
  const text=String(data);
  const transition=String(file).includes('/sessions/')&&text.includes('${NEW}')&&(text.includes('nativeId')||text.includes('pendingNativeSession'));
  const crash=fixtureCwd&&transition&&!fs.existsSync(fixtureCwd+'/.crashed')&&('${phase}'!=='committed'||text.includes('"nativeId"'));
  const kill=async()=>{fs.writeFileSync(fixtureCwd+'/.crashed','${phase}');await new Promise(resolve=>process.stdout.write(JSON.stringify({method:'fixture/crash',params:{}})+'\\n',resolve));process.kill(process.pid,'SIGKILL');};
  if(crash&&'${phase}'==='before')await kill();
  const result=await append(file,data,...args);
  if(crash&&'${phase}'!=='before'){if('${phase}'==='missing target'){const dir=path.join(process.env.CLAUDE_CONFIG_DIR,'projects',fixtureCwd.replace(/[^a-zA-Z0-9]/g,'-'));fs.rmSync(path.join(dir,'${NEW}.jsonl'),{force:true});}await kill();}
  return result;
};
`,
      )
      .replace(
        "  const abort=new AbortController();",
        "  const abort=new AbortController();fixtureCwd=options.cwd;",
      )
      .replace(
        "      yield {type:'system',subtype:'conversation_reset'",
        `
      yield {type:'system',subtype:'init',session_id:options.resume||options.sessionId};
      const file=path.join(process.env.CLAUDE_CONFIG_DIR,'projects',options.cwd.replace(/[^a-zA-Z0-9]/g,'-'),'${NEW}.jsonl');
      fs.mkdirSync(path.dirname(file),{recursive:true});
      fs.writeFileSync(file,JSON.stringify({type:'user',uuid:'44444444-4444-4444-8444-444444444444',parentUuid:null,sessionId:'${NEW}',cwd:options.cwd,isSidechain:false,timestamp:new Date().toISOString(),message:{role:'user',content:'new session anchor'}})+'\\n');
      yield {type:'system',subtype:'conversation_reset'`,
      );
    const base = path.join(
      os.homedir(),
      ".cache/codex-studio/pivot1-v5-history",
    );
    await fs.mkdir(base, { recursive: true });
    const bridge = await startBridge(crashSdk, "clear-crash-", {
      temporaryBase: base,
      env: { IDENTITY_SOURCE: phase === "unsolicited" ? "fork" : "both" },
    });
    try {
      const first = bridge
        .call("turn/start", {
          threadId: bridge.thread,
          input: [
            {
              type: "text",
              text: phase === "unsolicited" ? "continue normally" : "/clear",
            },
          ],
        })
        .catch(() => null);
      await bridge.note((note) => note.method === "fixture/crash");
      await first;
      await bridge.restart();
      let rejected;
      try {
        await bridge.call("turn/start", {
          threadId: bridge.thread,
          input: [{ type: "text", text: "after crash" }],
        });
      } catch (error) {
        rejected = error;
      }
      if (rejected)
        expect(rejected.message).toMatch(
          /native session.*unknown|transition.*unknown/i,
        );
      if (!rejected)
        await bridge.note(
          (note) =>
            note.method === "fixture/query" &&
            (note.params.resume || note.params.sessionId),
        );
      const queries = (
        await fs.readFile(path.join(bridge.root, ".queries"), "utf8")
      )
        .trim()
        .split("\n")
        .map(JSON.parse);
      expect(
        queries.slice(1).some((query) => query.resume === bridge.thread),
      ).toBe(false);
      if (phase === "after" || phase === "committed" || phase === "unsolicited")
        expect(
          (await bridge.call("claude/state", { threadId: bridge.thread }))
            .nativeId,
        ).toBe(NEW);
      else expect(rejected).toBeDefined();
    } finally {
      await bridge.close();
    }
  });

test("a rejected write-ahead save blocks live resume with an unknown transition", async () => {
  const { sessionStartHook, assertNativeSessionReady } =
    await import("./session-identity.mjs");
  const session = {
    id: "11111111-1111-4111-8111-111111111111",
    cwd: "/fixture",
    turns: [],
    pendingNativeSession: {
      from: "11111111-1111-4111-8111-111111111111",
      source: "input",
      status: "pending",
    },
  };
  const hook = sessionStartHook(
    session,
    () => undefined,
    async () => {
      throw new Error("save denied");
    },
  );
  await expect(
    hook(
      { hook_event_name: "SessionStart", source: "clear", session_id: NEW },
      undefined,
      { signal: new AbortController().signal },
    ),
  ).rejects.toThrow("save denied");
  expect(() => assertNativeSessionReady(session)).toThrow(
    /transition.*unknown/i,
  );
  expect(session.nativeId).toBeUndefined();
});

test("another new-session source durably saves target and identity before hook return", async () => {
  const { sessionStartHook } = await import("./session-identity.mjs");
  const OLD = "11111111-1111-4111-8111-111111111111";
  const session = { id: OLD, nativeId: OLD, cwd: "/fixture", turns: [] };
  const snapshots = [];
  let completedWrites = 0;
  const hook = sessionStartHook(
    session,
    () => undefined,
    async (state) => {
      snapshots.push(structuredClone(state));
      await Promise.resolve();
      completedWrites++;
    },
  );
  await hook(
    { hook_event_name: "SessionStart", source: "compact", session_id: NEW },
    undefined,
    { signal: new AbortController().signal },
  );
  expect(snapshots).toHaveLength(2);
  expect(snapshots[0].nativeId).toBe(OLD);
  expect(snapshots[0].pendingNativeSession.target).toBe(NEW);
  expect(snapshots[1].nativeId).toBe(NEW);
  expect(snapshots[1].pendingNativeSession).toBeUndefined();
  expect(completedWrites).toBe(2);
});

for (const deliveryStatus of ["not_applied", "cancelled"])
  for (const matches of [true, false])
    test(`saved ${deliveryStatus} receipt retires only its matching transition guard: ${matches}`, async () => {
      const { randomUUID } = await import("node:crypto");
      const { createSessionStore } = await import("./session-store.mjs");
      const { assertNativeSessionReady } =
        await import("./session-identity.mjs");
      const base = path.join(
        os.homedir(),
        ".cache/codex-studio/pivot1-v5-history",
      );
      await fs.mkdir(base, { recursive: true });
      const root = await fs.mkdtemp(path.join(base, "saved-guard-"));
      try {
        const id = randomUUID();
        const session = {
          id,
          cwd: root,
          started: true,
          pendingNativeSession: {
            from: id,
            source: "input",
            status: "pending",
            inputId: matches ? "clear-steer" : "other-steer",
          },
          turns: [
            {
              id: "turn",
              status: "completed",
              items: [
                {
                  id: "clear-steer",
                  type: "userMessage",
                  delivery: "steer",
                  deliveryStatus,
                },
              ],
            },
          ],
        };
        await createSessionStore(root).persist(session);
        const restored = await createSessionStore(root).get(id);
        if (matches) {
          expect(restored.pendingNativeSession).toBeUndefined();
          expect(() => assertNativeSessionReady(restored)).not.toThrow();
          expect(
            (await createSessionStore(root).get(id)).pendingNativeSession,
          ).toBeUndefined();
        } else
          expect(() => assertNativeSessionReady(restored)).toThrow(/unknown/);
      } finally {
        await fs.rm(root, { recursive: true, force: true });
      }
    });
