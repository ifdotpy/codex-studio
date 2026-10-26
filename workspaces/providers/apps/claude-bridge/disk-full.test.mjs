// A full disk (ENOSPC) at the session-store write boundary. Faults are
// injected into node:fs/promises; no test fills a real disk.
import { afterEach, test, vi } from "vitest";
import assert from "node:assert/strict";
import fs from "node:fs/promises";
import { existsSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import { createSessionStore, isDiskFull } from "./session-store.ts";
import { startBridge } from "./testing/bridge-harness.mjs";

const sessionId = "11111111-1111-4111-8111-111111111111";
const makeSession = () => ({
  id: sessionId,
  cwd: "/tmp/work",
  turns: [
    {
      id: "turn-1",
      status: "completed",
      items: [{ id: "user-1", content: [{ type: "text", text: "hello" }] }],
    },
  ],
});
const withRoot = async (run) => {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), "claude-disk-full-"));
  try {
    await run(root);
  } finally {
    await fs.rm(root, { recursive: true, force: true });
  }
};
const diskFull = (target) =>
  Object.assign(
    new Error("ENOSPC: no space left on device, rename '" + target + "'"),
    { code: "ENOSPC" },
  );
const temporaryFiles = async (directory) =>
  (await fs.readdir(directory)).filter((name) => name.endsWith(".tmp"));

test("disk-full detection follows error codes, causes and aggregate errors", () => {
  assert.equal(isDiskFull(diskFull("x")), true);
  assert.equal(isDiskFull({ code: "EDQUOT" }), true);
  assert.equal(isDiskFull(new Error("outer", { cause: diskFull("x") })), true);
  assert.equal(isDiskFull(new AggregateError([diskFull("x")], "both")), true);
  assert.equal(isDiskFull({ code: "EIO" }), false);
  assert.equal(isDiskFull(null), false);
});

test("a disk-full metadata rename leaves no temporary file and keeps the previous file", async () =>
  withRoot(async (root) => {
    const store = createSessionStore(root, {
      compactRecords: 1000,
      diskFullRetryMs: 60_000,
    });
    const session = makeSession();
    session.preview = "old preview";
    await store.persist(session);
    const metadataFile = path.join(root, "sessions", sessionId + ".meta.json");
    const before = await fs.readFile(metadataFile, "utf8");
    session.preview = "new preview";
    const originalRename = fs.rename;
    fs.rename = async (from, to) => {
      if (String(to).endsWith(".meta.json")) throw diskFull(to);
      return originalRename(from, to);
    };
    try {
      await assert.rejects(store.persist(session), /ENOSPC/);
    } finally {
      fs.rename = originalRename;
    }
    assert.deepEqual(await temporaryFiles(path.join(root, "sessions")), []);
    assert.equal(await fs.readFile(metadataFile, "utf8"), before);
    assert.equal(store.isUnsaved(sessionId), true);
    assert.equal(store.stats().unsavedSessions, 1);
    assert.equal(
      await store.evict(sessionId),
      false,
      "an unsaved session stays cached until it is saved",
    );
    await store.drain();
    assert.equal(store.isUnsaved(sessionId), false);
    const saved = await createSessionStore(root).metadata(sessionId);
    assert.equal(saved.preview, "new preview");
  }));

test("a partial disk-full snapshot write removes its temporary file", async () =>
  withRoot(async (root) => {
    const store = createSessionStore(root, { diskFullRetryMs: 60_000 });
    const originalWrite = fs.writeFile;
    fs.writeFile = async (file, data, ...rest) => {
      if (String(file).endsWith(".tmp")) {
        await originalWrite(file, String(data).slice(0, 10), ...rest);
        throw diskFull(file);
      }
      return originalWrite(file, data, ...rest);
    };
    try {
      await assert.rejects(store.persist(makeSession()), /ENOSPC/);
    } finally {
      fs.writeFile = originalWrite;
    }
    assert.deepEqual(await temporaryFiles(path.join(root, "sessions")), []);
    await assert.rejects(
      fs.readFile(path.join(root, "sessions", sessionId + ".json")),
      { code: "ENOENT" },
    );
  }));

test("persistOrDefer saves again when space returns and rejects other errors", async () =>
  withRoot(async (root) => {
    let onSaved;
    const saved = new Promise((resolve) => {
      onSaved = resolve;
    });
    const store = createSessionStore(root, {
      compactRecords: 1000,
      diskFullRetryMs: 20,
      onSaved,
    });
    const session = makeSession();
    await store.persist(session);
    session.turns[0].items.push({ id: "answer-1", text: "kept in memory" });
    const originalAppend = fs.appendFile;
    let full = true;
    fs.appendFile = async (file, ...rest) => {
      if (full && String(file).endsWith(".jsonl")) throw diskFull(file);
      return originalAppend(file, ...rest);
    };
    try {
      vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
      assert.equal(await store.persistOrDefer(session), true);
      assert.equal(store.isUnsaved(sessionId), true);
      full = false;
      await vi.advanceTimersByTimeAsync(20);
      await saved;
    } finally {
      fs.appendFile = originalAppend;
      vi.useRealTimers();
    }
    assert.equal(store.isUnsaved(sessionId), false);
    assert.deepEqual(
      (await createSessionStore(root).readAll(sessionId)).session,
      session,
      "the retry saves the live session from the last saved state",
    );
    fs.appendFile = async () => {
      throw Object.assign(new Error("injected I/O failure"), { code: "EIO" });
    };
    session.turns[0].items.push({ id: "answer-2", text: "not saved" });
    try {
      await assert.rejects(store.persistOrDefer(session), /injected I\/O/);
    } finally {
      fs.appendFile = originalAppend;
    }
    assert.equal(store.isUnsaved(sessionId), false);
  }));

// The scripted SDK patches node:fs/promises in the bridge process: while
// `.disk-full` exists in the bridge folder, every rename fails with ENOSPC, as
// in the incident. Turn "fill" creates `.disk-full` after Claude starts, sends
// an answer, waits for `.space`, removes `.disk-full`, and ends successfully.
const FAKE_SDK = String.raw`
import fs from 'node:fs';
import fsp from 'node:fs/promises';
const marker=process.cwd()+'/.disk-full';
const rename=fsp.rename;
fsp.rename=async(from,to)=>{
 if(fs.existsSync(marker))throw Object.assign(new Error("ENOSPC: no space left on device, rename '"+from+"' -> '"+to+"'"),{code:'ENOSPC'});
 return rename(from,to);
};
export const tool=(name,description,schema,call)=>({name,description,schema,call});
export const createSdkMcpServer=value=>({...value});
export const forkSession=async()=>({sessionId:'22222222-2222-4222-8222-222222222222'});
export const getSessionMessages=async()=>[];
const until=(file,signal)=>waitForFile(file,{signal}).catch(()=>{});
const usage={input_tokens:10,output_tokens:2};
export function query({prompt,options}){
 const abort=new AbortController(),cwd=options.cwd;
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
   fs.appendFileSync(cwd+'/.inputs',input.value.message.content[0].text+'\n');
   yield {type:'user',isReplay:true,uuid:input.value.uuid};
   yield {type:'system',subtype:'init'};
   fs.writeFileSync(marker,'');
   yield {type:'stream_event',event:{type:'message_start',message:{id:'message-1'}}};
   yield {type:'assistant',uuid:'a-1',message:{id:'message-1',model:'claude-opus-5-5',usage,content:[{type:'text',text:'still working'}]}};
   await until(cwd+'/.space',abort.signal);
   if(abort.signal.aborted)return;
   fs.rmSync(marker,{force:true});
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

test("a disk-full session save does not fail the running turn and loses nothing", async () => {
  const bridge = await startBridge(FAKE_SDK, "claude-disk-full-");
  bridges.push(bridge);
  const sessions = path.join(bridge.root, "state", "sessions");
  const turn = (
    await bridge.call("turn/start", {
      threadId: bridge.thread,
      clientUserMessageId: "fill-1",
      input: [{ type: "text", text: "fill" }],
    })
  ).turn.id;
  const held = await bridge.note(
    (n) =>
      n.method === "item/completed" && n.params.item.id === "disk-full:" + turn,
  );
  assert.match(held.params.item.text, /^Disk is full\./);
  assert.equal(held.params.item.studioNotice, true);
  // Saves run beside the stream and keep failing while the disk is full, so
  // a temporary file may exist now by design. The turn's end waits for its
  // saves; only then does a leaked temporary file show (checked below).
  await fs.writeFile(path.join(bridge.root, ".space"), "");
  const completed = await bridge.note((n) => n.method === "turn/completed");
  assert.equal(completed.params.turn.id, turn);
  assert.equal(completed.params.turn.status, "completed");
  assert.equal(completed.params.turn.error, undefined);
  const saved = await createSessionStore(
    path.join(bridge.root, "state"),
  ).readAll(bridge.thread);
  const savedTurn = saved.session.turns.find((item) => item.id === turn);
  assert.equal(savedTurn.status, "completed");
  assert.ok(
    savedTurn.items.some((item) => item.text === "still working"),
    "the answer given while the disk was full is saved",
  );
  assert.deepEqual(
    await temporaryFiles(sessions),
    [],
    "a failed rename leaves no temporary file",
  );
});

test("a disk-full pre-input receipt rejects the start before Claude receives input", async () => {
  const bridge = await startBridge(FAKE_SDK, "claude-disk-full-start-");
  bridges.push(bridge);
  await fs.writeFile(path.join(bridge.root, ".disk-full"), "");
  await assert.rejects(
    bridge.call("turn/start", {
      threadId: bridge.thread,
      clientUserMessageId: "blocked-1",
      input: [{ type: "text", text: "fill" }],
    }),
    /^Error: Disk is full\. Claude did not receive this input\./,
  );
  assert.equal(existsSync(path.join(bridge.root, ".inputs")), false);
  assert.deepEqual(
    await temporaryFiles(path.join(bridge.root, "state", "sessions")),
    [],
  );
  await fs.rm(path.join(bridge.root, ".disk-full"));
  const read = await bridge.call("thread/read", {
    threadId: bridge.thread,
    includeTurns: true,
  });
  assert.deepEqual(read.thread.turns, []);
});
