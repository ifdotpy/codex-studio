import assert from "node:assert/strict";
import fs from "node:fs/promises";
import path from "node:path";
import sharp from "sharp";
import { test } from "vitest";
import { startBridge } from "./testing/bridge-harness.ts";
import { MAX_IMAGE_SIDE, MAX_IMAGE_BASE64_BYTES } from "./images.mjs";

const SDK = String.raw`
import fs from 'node:fs';
export const tool=(name,description,schema,call)=>({name,description,schema,call});
export const createSdkMcpServer=value=>({...value});
export const forkSession=async()=>({sessionId:'22222222-2222-4222-8222-222222222222'});
export const getSessionMessages=async()=>[];
export function query({prompt,options}){
 fs.appendFileSync(options.cwd+'/.queries','q');
 return {
  supportedModels:async()=>[{value:'default',displayName:'Default',resolvedModel:'claude-opus-5-5'}],
  accountInfo:async()=>({email:'test@example.test',subscriptionType:'Claude Max',apiProvider:'firstParty'}),
  initializationResult:async()=>({commands:[]}),mcpServerStatus:async()=>[],
  setModel:async()=>{},setPermissionMode:async()=>{},applyFlagSettings:async()=>{},
  setMcpServers:async()=>({added:[],removed:[],errors:{}}),
  close(){},interrupt:async()=>{},
  async *[Symbol.asyncIterator](){
   for await(const input of prompt){
    process.stdout.write(JSON.stringify({method:'fixture/input',params:input.message})+'\n');
    yield {type:'user',isReplay:true,uuid:input.uuid};
    if(input.message.content[0]?.text==='hold')await waitForFile(options.cwd+'/release');
    yield {type:'result',subtype:'success',usage:{input_tokens:1,output_tokens:1},result:'done'};
   }
  }
 };
}
`;

test("image refusal reaches the caller before a turn or SDK query exists", async () => {
  const bridge = await startBridge(SDK, "claude-image-refusal-");
  try {
    const file = path.join(bridge.root, "broken.png");
    await fs.writeFile(file, "broken");
    await assert.rejects(
      bridge.call("turn/start", {
        threadId: bridge.thread,
        clientUserMessageId: "refused-image",
        input: [
          { type: "text", text: "Read this image" },
          { type: "localImage", path: file },
        ],
      }),
      /broken.png: Cannot send image.*could not read or compress/,
    );
    assert.equal(
      (
        await bridge.call("thread/read", {
          threadId: bridge.thread,
          includeTurns: true,
        })
      ).thread.turns.length,
      0,
    );
    await assert.rejects(fs.stat(path.join(bridge.root, ".queries")), {
      code: "ENOENT",
    });
    assert.equal(
      bridge.notes.some((note) => note.method === "turn/started"),
      false,
    );
  } finally {
    await bridge.close();
  }
});

test("missing native codecs refuse images while the bridge can still accept text", async () => {
  const bridge = await startBridge(SDK, "claude-image-missing-codec-");
  try {
    const file = path.join(bridge.root, "valid.png");
    await sharp({
      create: { width: 1, height: 1, channels: 3, background: "red" },
    })
      .png()
      .toFile(file);
    // Only this fixture's dependency link changes. The SDK is scripted locally.
    await fs.unlink(path.join(bridge.root, "node_modules"));
    await assert.rejects(
      bridge.call("turn/start", {
        threadId: bridge.thread,
        input: [{ type: "localImage", path: file }],
      }),
      /valid.png: Cannot send image.*processor is unavailable/,
    );
    await bridge.call("turn/start", {
      threadId: bridge.thread,
      input: [{ type: "text", text: "hello" }],
    });
    assert.deepEqual(
      (await bridge.note((note) => note.method === "fixture/input")).params
        .content,
      [{ type: "text", text: "hello" }],
    );
  } finally {
    await bridge.close();
  }
});

test("the real image caller sends a bounded base64 image with its actual MIME type", async () => {
  const bridge = await startBridge(SDK, "claude-image-input-");
  try {
    const file = path.join(bridge.root, "wide.gif");
    await sharp({
      create: { width: 2400, height: 1200, channels: 3, background: "red" },
    })
      .gif()
      .toFile(file);
    const result = await bridge.call("turn/start", {
      threadId: bridge.thread,
      input: [{ type: "localImage", path: file }],
    });
    const input = await bridge.note((note) => note.method === "fixture/input");
    const [block] = input.params.content;
    assert.equal(block.type, "image");
    assert.equal(block.source.media_type, "image/png");
    assert.ok(block.source.data.length <= MAX_IMAGE_BASE64_BYTES);
    const metadata = await sharp(
      Buffer.from(block.source.data, "base64"),
    ).metadata();
    assert.equal(metadata.width, MAX_IMAGE_SIDE);
    await bridge.note(
      (note) =>
        note.method === "turn/completed" &&
        note.params.turn.id === result.turn.id,
    );
  } finally {
    await bridge.close();
  }
});

test("a refused image steer leaves the active turn unchanged", async () => {
  const bridge = await startBridge(SDK, "claude-image-steer-");
  try {
    const { turn } = await bridge.call("turn/start", {
      threadId: bridge.thread,
      input: [{ type: "text", text: "hold" }],
    });
    await bridge.note((note) => note.method === "fixture/input");
    const file = path.join(bridge.root, "broken.gif");
    await fs.writeFile(file, "broken");
    await assert.rejects(
      bridge.call("turn/steer", {
        threadId: bridge.thread,
        expectedTurnId: turn.id,
        clientUserMessageId: "refused-steer",
        input: [{ type: "localImage", path: file }],
      }),
      /broken.gif: Cannot send image/,
    );
    const saved = await bridge.call("thread/read", {
      threadId: bridge.thread,
      includeTurns: true,
    });
    assert.equal(saved.thread.turns[0].status, "inProgress");
    assert.equal(
      saved.thread.turns[0].items.some((item) => item.id === "refused-steer"),
      false,
    );
    await bridge.writeSignal("release");
    const finished = await bridge.note(
      (note) =>
        note.method === "turn/completed" && note.params.turn.id === turn.id,
    );
    assert.equal(finished.params.turn.status, "completed");
  } finally {
    await bridge.close();
  }
});
