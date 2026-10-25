import { expect, test } from "vitest";
import { prepareIdleSummary, applyIdleSummary } from "./idle-summary.mjs";
import { promptPrefix } from "./idle-compaction.mjs";

function fixture() {
  const source = [
    { type: "user", uuid: "u", message: { content: "hello" } },
    { type: "assistant", uuid: "a", message: { content: "answer" } },
  ];
  const s = {
    id: "studio",
    nativeId: "main",
    cwd: "/repo",
    started: true,
    turns: [{ id: "turn", status: "completed", items: [] }],
    idleCache: { requestId: "response" },
  };
  let forks = 0;
  const writes = [];
  const deps = {
    persist: async (value) => writes.push(structuredClone(value)),
    getSessionMessages: async (id) =>
      id === "main"
        ? source
        : source.map((m) => ({ ...m, uuid: m.uuid + "-fork" })),
    forkSession: async () => {
      forks++;
      return { sessionId: "fork" };
    },
    compact: async () => ({
      trigger: "manual",
      pre_tokens: 150000,
      post_tokens: 1000,
    }),
    isCurrent: () => true,
  };
  const expected = { requestId: "response", prefix: promptPrefix(s) };
  return { s, source, deps, expected, writes, forks: () => forks };
}

test("preparation keeps the main history; return commits the fork exactly once", async () => {
  const f = fixture();
  await prepareIdleSummary(f.s, "idle:one", f.expected, {}, f.deps);
  expect(f.s.nativeId).toBe("main");
  expect(f.s.idleSummary.status).toBe("ready");
  expect(f.writes[0].idleSummary.status).toBe("preparing");
  const restarted = structuredClone(f.s);
  expect(await applyIdleSummary(restarted, {}, f.deps)).toMatchObject({
    requestId: "idle:one",
  });
  expect(restarted.nativeId).toBe("fork");
  expect(restarted.historyBranches[0].nativeId).toBe("main");
  expect(await applyIdleSummary(restarted, {}, f.deps)).toBe(null);
  expect(f.forks()).toBe(1);
});

test("changed history, prefix, and native identity prevent replacement", async () => {
  for (const change of [
    (f) =>
      f.source.push({
        type: "user",
        uuid: "new",
        message: { content: "new input" },
      }),
    (f) => {
      f.s.model = "different";
    },
    (f) => {
      f.s.nativeId = "other";
    },
  ]) {
    const f = fixture();
    await prepareIdleSummary(f.s, "idle:one", f.expected, {}, f.deps);
    change(f);
    const before = f.s.nativeId;
    expect(await applyIdleSummary(f.s, {}, f.deps)).toBe(null);
    expect(f.s.nativeId).toBe(before);
  }
});

test("failed fork or summary and uncertain preparation never replace or retry", async () => {
  for (const stage of ["forkSession", "compact", "persist"]) {
    const f = fixture();
    f.deps[stage] = async () => {
      throw new Error(stage);
    };
    await expect(
      prepareIdleSummary(f.s, "idle:one", f.expected, {}, f.deps),
    ).rejects.toThrow(stage);
    expect(f.s.nativeId).toBe("main");
    await expect(
      prepareIdleSummary(f.s, "idle:one", f.expected, {}, f.deps),
    ).rejects.toThrow();
    expect(f.forks()).toBeLessThanOrEqual(1);
  }
});

test("the fork must preserve each message, and a busy source cannot become ready", async () => {
  const f = fixture();
  f.deps.getSessionMessages = async (id) => (id === "main" ? f.source : []);
  await expect(
    prepareIdleSummary(f.s, "idle:one", f.expected, {}, f.deps),
  ).rejects.toThrow("history");
  expect(f.s.nativeId).toBe("main");
  const g = fixture();
  g.deps.compact = async () => {
    g.deps.isCurrent = () => false;
    return {};
  };
  await expect(
    prepareIdleSummary(g.s, "idle:one", g.expected, {}, g.deps),
  ).rejects.toThrow("changed");
  expect(g.s.idleSummary.status).not.toBe("ready");
});

test.each([true, false])(
  "the real bridge resumes the prepared fork only with a current roster proof: %s",
  async (rosterProof) => {
    const { startBridge } = await import("./testing/bridge-harness.mjs");
    const sdk = String.raw`
import fs from 'node:fs/promises';
const rows=[{type:'user',uuid:'user',message:{role:'user',content:'hello'}},{type:'assistant',uuid:'answer',message:{role:'assistant',content:'answer'}}];
export const resolveSettings=async()=>({effective:{}});
export const tool=(name,description,schema,call)=>({name,description,schema,call});
export const createSdkMcpServer=value=>({...value});
export const forkSession=async()=>({sessionId:'22222222-2222-4222-8222-222222222222'});
export const getSessionMessages=async(id)=>rows.map(row=>({...row,session_id:id,uuid:row.uuid+id}));
export function query({prompt,options}) {
 const abort=new AbortController();
 return {supportedModels:async()=>[{value:'default',displayName:'Default',resolvedModel:'claude-opus-5-5'}],
 accountInfo:async()=>({email:'test@example.test',subscriptionType:'Claude Max',apiProvider:'firstParty'}),
 initializationResult:async()=>({commands:[]}),mcpServerStatus:async()=>[],reconnectMcpServer:async()=>{},setModel:async()=>{},setPermissionMode:async()=>{},applyFlagSettings:async()=>{},stopTask:async()=>{},setMcpServers:async()=>({added:[],removed:[],errors:{}}),
 close(){abort.abort();},interrupt:async()=>abort.abort(),
 async *[Symbol.asyncIterator]() {
  for await (const input of prompt) {
   if(abort.signal.aborted)return;
   const text=input.message.content[0].text;
   await fs.appendFile(new URL('./submitted.jsonl',import.meta.url),JSON.stringify({nativeId:options.resume||options.sessionId,text,content:input.message.content})+'\n');
   yield {type:'user',isReplay:true,uuid:input.uuid};
   if(input.shouldQuery===false)continue;
   yield {type:'system',subtype:'init'};
   if(text==='/compact')yield {type:'system',subtype:'compact_boundary',uuid:'boundary',compact_metadata:{trigger:'manual',pre_tokens:150000,post_tokens:1000}};
   yield {type:'assistant',uuid:'message',message:{id:'response',role:'assistant',model:'claude-opus-5-5',content:[{type:'text',text:'done'}],usage:{input_tokens:10,cache_read_input_tokens:150000,cache_creation_input_tokens:1000,output_tokens:20,cache_creation:{ephemeral_5m_input_tokens:1000}}}};
   yield {type:'result',subtype:'success',usage:{input_tokens:10,output_tokens:20},result:'done'};
  }
 }};
}
`;
    const bridge = await startBridge(sdk, "claude-summary-return-");
    const fs = await import("node:fs/promises");
    try {
      const first = await bridge.call("turn/start", {
        threadId: bridge.thread,
        clientUserMessageId: "first",
        input: [{ type: "text", text: "hello" }],
      });
      const {
        params: { report },
      } = await bridge.note(
        (note) => note.method === "thread/idleCompaction/updated",
      );
      await bridge.note(
        (note) =>
          note.method === "turn/completed" &&
          note.params.turn.id === first.turn.id,
      );
      const prepared = await bridge.call("thread/compact/start", {
        threadId: bridge.thread,
        requestId: "idle-compaction:prepare",
        idleCompaction: { ...report, mode: "compact_on_return" },
      });
      await bridge.note(
        (note) =>
          note.method === "turn/completed" &&
          note.params.turn.id === prepared.turn.id,
      );
      expect(
        bridge.notes.filter(
          (note) =>
            note.method === "item/completed" &&
            note.params.item.type === "contextCompaction",
        ),
      ).toHaveLength(0);
      const state = await bridge.call("claude/state", {
        threadId: bridge.thread,
      });
      expect(state.nativeId).toBe(bridge.thread);
      await bridge.restart();
      const currentRoster =
        '[Studio team state after compaction]\n{"children":[],"tasks":[{"owner":"worker","status":"review"}],"pendingResults":[{"id":"worker-finished"}]}';
      const next = {
        threadId: bridge.thread,
        clientUserMessageId: "return",
        idleCompactionMode: "compact_on_return",
        idleSummaryRosterIncluded: rosterProof,
        input: [
          { type: "text", text: "continue" },
          { type: "text", text: currentRoster, inputOrigin: "studio_event" },
        ],
      };
      const returned = await bridge.call("turn/start", next);
      await bridge.note(
        (note) =>
          note.method === "turn/completed" &&
          note.params.turn.id === returned.turn.id,
      );
      await bridge.call("turn/start", next);
      const submissions = (
        await fs.readFile(bridge.root + "/submitted.jsonl", "utf8")
      )
        .trim()
        .split("\n")
        .map(JSON.parse);
      expect(submissions.map((row) => row.text)).toEqual([
        "hello",
        "/compact",
        "continue",
        currentRoster,
      ]);
      expect(submissions[1].nativeId).toBe(
        "22222222-2222-4222-8222-222222222222",
      );
      expect(submissions[2].nativeId).toBe(
        rosterProof ? submissions[1].nativeId : bridge.thread,
      );
      expect(submissions[3].nativeId).toBe(submissions[2].nativeId);
      expect(JSON.stringify(submissions[3].content)).toContain(
        "worker-finished",
      );
      expect(JSON.stringify(submissions[3].content)).toContain("review");
      expect(
        bridge.notes.filter(
          (note) =>
            note.method === "item/completed" &&
            note.params.item.idleCompactionOnReturn,
        ),
      ).toHaveLength(rosterProof ? 1 : 0);
    } finally {
      await bridge.close();
    }
  },
);

test("a lost target or changed activity during validation keeps the original", async () => {
  for (const kind of ["missing", "busy"]) {
    const f = fixture();
    await prepareIdleSummary(f.s, "idle:one", f.expected, {}, f.deps);
    const read = f.deps.getSessionMessages;
    f.deps.getSessionMessages = async (id) => {
      if (id === "fork") {
        if (kind === "missing") throw new Error("target missing");
        f.deps.isCurrent = () => false;
      }
      return read(id);
    };
    expect(await applyIdleSummary(f.s, {}, f.deps)).toBe(null);
    expect(f.s.nativeId).toBe("main");
  }
});

test("a failed switch write restores all memory fields before another input", async () => {
  const f = fixture();
  await prepareIdleSummary(f.s, "idle:one", f.expected, {}, f.deps);
  f.deps.persist = async () => {
    throw new Error("disk");
  };
  await expect(applyIdleSummary(f.s, {}, f.deps)).rejects.toThrow("disk");
  expect(f.s.nativeId).toBe("main");
  expect(f.s.historyBranches).toBeUndefined();
  expect(f.s.idleSummary.status).toBe("ready");
});

test("the installed SDK fork preserves the proof for native message and system rows", async () => {
  const sdk = await import("@anthropic-ai/claude-agent-sdk");
  const main = "11111111-1111-4111-8111-111111111111";
  const input = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
  const answer = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
  const rows = [
    {
      type: "user",
      uuid: input,
      parentUuid: null,
      sessionId: main,
      cwd: "/repo",
      timestamp: "2026-10-25T00:00:00.000Z",
      message: { role: "user", content: "hello" },
    },
    {
      type: "system",
      uuid: "cccccccc-cccc-4ccc-8ccc-cccccccccccc",
      parentUuid: input,
      sessionId: main,
      cwd: "/repo",
      timestamp: "2026-10-25T00:00:01.000Z",
      subtype: "local_command",
      message: { content: "native notice" },
    },
    {
      type: "assistant",
      uuid: answer,
      parentUuid: "cccccccc-cccc-4ccc-8ccc-cccccccccccc",
      sessionId: main,
      cwd: "/repo",
      timestamp: "2026-10-25T00:00:02.000Z",
      message: {
        role: "assistant",
        content: [{ type: "text", text: "answer" }],
      },
    },
  ];
  const saved = new Map([[main, rows]]);
  const sessionStore = {
    load: async (key) => structuredClone(saved.get(key.sessionId) || null),
    append: async (key, entries) =>
      saved.set(key.sessionId, [
        ...(saved.get(key.sessionId) || []),
        ...structuredClone(entries),
      ]),
  };
  const source = await sdk.getSessionMessages(main, {
    dir: "/repo",
    sessionStore,
    includeSystemMessages: true,
  });
  expect(source.map((message) => message.type)).toEqual([
    "user",
    "system",
    "assistant",
  ]);
  const s = {
    id: "studio",
    nativeId: main,
    started: true,
    cwd: "/repo",
    turns: [],
    idleCache: { requestId: "response" },
  };
  const receipt = await prepareIdleSummary(
    s,
    "idle:sdk",
    { requestId: "response", prefix: promptPrefix(s) },
    {},
    {
      persist: async () => {},
      isCurrent: () => true,
      getSessionMessages: (id, options) =>
        sdk.getSessionMessages(id, { ...options, sessionStore }),
      forkSession: (id, options) =>
        sdk.forkSession(id, { ...options, sessionStore }),
      compact: async () => ({ trigger: "manual" }),
    },
  );
  expect(receipt.status).toBe("ready");
  expect(receipt.targetNativeId).not.toBe(main);
  expect(s.nativeId).toBe(main);
});
