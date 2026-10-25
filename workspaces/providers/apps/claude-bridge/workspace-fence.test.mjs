// Exercise thread start/resume through the real bridge with an SDK fixture.
import { afterEach, test } from "vitest";
import assert from "node:assert/strict";
import fs from "node:fs/promises";
import path from "node:path";
import { startBridge } from "./testing/bridge-harness.mjs";

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

test("native Layr sessions resolve the managed binary and receive their store environment", async () => {
  const sdk = FAKE_SDK.replace(
    "import path from 'node:path';",
    "import path from 'node:path'; import {spawnSync} from 'node:child_process';",
  ).replace(
    "account:options.env.STUDIO_CLAUDE_ACCOUNT,",
    "account:options.env.STUDIO_CLAUDE_ACCOUNT, native: {version:spawnSync('layr',['--version'],{env:options.env,encoding:'utf8'}).stdout,root:options.env.LAYR_ROOT,direct:options.env.LAYR_DIRECT,socket:options.env.LAYR_SOCKET,tmp:options.env.TMPDIR},",
  );
  const bridge = await startBridge(sdk, "claude-native-layr-");
  bridges.push(bridge);
  const bin = path.join(bridge.root, "managed bin");
  await fs.mkdir(bin);
  await fs.writeFile(
    path.join(bin, "layr"),
    "#!/bin/sh\nprintf 'layr version 0.2.0 managed\\n'\n",
    { mode: 0o755 },
  );
  const environment = {
    PATH: bin + path.delimiter + process.env.PATH,
    LAYR_ROOT: path.join(bridge.root, "store"),
    LAYR_DIRECT: "0",
    LAYR_SOCKET: path.join(bridge.root, "layr.sock"),
    TMPDIR: bridge.root,
  };
  const browserBin = path.join(bridge.root, "browser bin");
  await fs.mkdir(browserBin);
  await fs.writeFile(
    path.join(bridge.root, "state", "studio-browser.json"),
    JSON.stringify({
      BROWSER: "fixture-browser",
      STUDIO_BROWSER_SOCKET: path.join(bridge.root, "browser.sock"),
      STUDIO_BROWSER_PATH: browserBin,
    }),
  );
  const options = {
    cwd: bridge.root,
    approvalPolicy: "never",
    claude: { env: environment },
  };
  const id = (await bridge.call("thread/start", options)).thread.id;
  for (let attempt = 0; attempt < 2; attempt++) {
    if (attempt)
      await bridge.call("thread/resume", { threadId: id, ...options });
    const turn = (
      await bridge.call("turn/start", {
        threadId: id,
        clientUserMessageId: `native-${attempt}`,
        input: [{ type: "text", text: "Check native environment" }],
      })
    ).turn.id;
    await bridge.note(
      (n) => n.method === "turn/completed" && n.params.turn.id === turn,
    );
    const actual = JSON.parse(
      await fs.readFile(path.join(bridge.root, "sdk-env.json"), "utf8"),
    ).native;
    assert.deepEqual(actual, {
      version: "layr version 0.2.0 managed\n",
      root: environment.LAYR_ROOT,
      direct: "0",
      socket: environment.LAYR_SOCKET,
      tmp: bridge.root,
    });
  }
});

test("native reviewers keep the main store outside their writable sandbox", async () => {
  const sdk = FAKE_SDK.replace(
    "account:options.env.STUDIO_CLAUDE_ACCOUNT,",
    "account:options.env.STUDIO_CLAUDE_ACCOUNT, sandbox:options.sandbox,",
  );
  const bridge = await startBridge(sdk, "claude-native-reviewer-");
  bridges.push(bridge);
  const scratch = path.join(bridge.root, "scratch");
  await fs.mkdir(scratch);
  const sandbox = {
    enabled: true,
    failIfUnavailable: true,
    allowUnsandboxedCommands: false,
    filesystem: {
      allowWrite: [scratch],
      denyWrite: [path.join(bridge.root, "store")],
    },
    network: {
      allowedDomains: [],
      strictAllowlist: true,
      allowUnixSockets: [],
      allowAllUnixSockets: false,
    },
  };
  const id = (
    await bridge.call("thread/start", {
      cwd: scratch,
      approvalPolicy: "on-request",
      sandbox: "read-only",
      claude: { permissionMode: "plan", sandbox },
    })
  ).thread.id;
  const turn = (
    await bridge.call("turn/start", {
      threadId: id,
      clientUserMessageId: "native-review",
      input: [{ type: "text", text: "Read only" }],
    })
  ).turn.id;
  await bridge.note(
    (n) => n.method === "turn/completed" && n.params.turn.id === turn,
  );
  assert.deepEqual(
    JSON.parse(await fs.readFile(path.join(scratch, "sdk-env.json"), "utf8"))
      .sandbox,
    sandbox,
  );
});

test("native reviewer sessions retain account selectors with a filtered environment", async () => {
  const sdk = FAKE_SDK.replace(
    "account:options.env.STUDIO_CLAUDE_ACCOUNT,",
    "account:options.env.STUDIO_CLAUDE_ACCOUNT, environment:options.env,",
  );
  const bridge = await startBridge(sdk, "claude-reviewer-env-", {
    env: {
      STUDIO_REVIEW_TEST_SECRET: "fixture-only-secret",
      STUDIO_CLAUDE_BIN: "fixture-cli",
      STUDIO_CLAUDE_CREDENTIAL_IDENTITY: "claude:test@example.test",
    },
  });
  bridges.push(bridge);
  const scratch = path.join(bridge.root, "scratch");
  await fs.mkdir(scratch);
  const environment = {
    PATH: process.env.PATH,
    HOME: scratch,
    TMPDIR: scratch,
    LANG: "C",
    TERM: "dumb",
  };
  const id = (
    await bridge.call("thread/start", {
      cwd: scratch,
      claude: {
        permissionMode: "plan",
        nativeReviewerEnvironment: environment,
      },
    })
  ).thread.id;
  for (let attempt = 0; attempt < 2; attempt++) {
    if (attempt)
      await bridge.call("thread/resume", {
        threadId: id,
        cwd: scratch,
        claude: {
          permissionMode: "plan",
          nativeReviewerEnvironment: environment,
        },
      });
    const turn = (
      await bridge.call("turn/start", {
        threadId: id,
        clientUserMessageId: `review-env-${attempt}`,
        input: [{ type: "text", text: "Inspect environment" }],
      })
    ).turn.id;
    await bridge.note(
      (n) => n.method === "turn/completed" && n.params.turn.id === turn,
    );
    const actual = JSON.parse(
      await fs.readFile(path.join(scratch, "sdk-env.json"), "utf8"),
    ).environment;
    assert.equal(
      actual.CLAUDE_CONFIG_DIR,
      path.join(bridge.root, "native-config"),
    );
    assert.equal(actual.STUDIO_CLAUDE_ACCOUNT, "test@example.test");
    assert.equal(actual.STUDIO_CLAUDE_BIN, "fixture-cli");
    assert.equal(
      actual.STUDIO_CLAUDE_CREDENTIAL_IDENTITY,
      "claude:test@example.test",
    );
    assert.deepEqual(
      Object.keys(actual).sort(),
      [
        ...Object.keys(environment),
        "CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING",
        "CLAUDE_CONFIG_DIR",
        "STUDIO_CLAUDE_ACCOUNT",
        "STUDIO_CLAUDE_BIN",
        "STUDIO_CLAUDE_CREDENTIAL_IDENTITY",
      ].sort(),
    );
    assert.equal(actual.CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING, "true");
    for (const key of Object.keys(environment))
      assert.equal(
        actual[key],
        key === "HOME" ? process.env.HOME : environment[key],
      );
  }
});
