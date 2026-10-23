import { afterEach, test } from "vitest";
import assert from "node:assert/strict";
import fs from "node:fs/promises";
import path from "node:path";
import { startBridge } from "./testing/bridge-harness.mjs";
import {
  capabilityToolOptions,
  capabilityToolDenial,
} from "./worker-capabilities.mjs";

const FAKE_SDK = String.raw`
import {appendFileSync, readFileSync, writeFileSync, existsSync} from 'node:fs';
import path from 'node:path';
export const tool=(name,description,schema,call)=>({name,description,schema,call});
export const createSdkMcpServer=value=>({...value});
export const forkSession=async()=>({sessionId:'fork'});
export const getSessionMessages=async()=>[];
export function query({prompt,options}){
 const session=options.sessionId || options.resume;
 const log=(event)=>appendFileSync(path.join(options.cwd,'sdk-events.jsonl'),JSON.stringify({session,...event})+'\n');
 if(session)log({kind:'options',options});
 const abort=new AbortController();
 const status=()=>{
  const file=path.join(options.cwd,'mcp-status.json');
  return existsSync(file)?JSON.parse(readFileSync(file,'utf8')):Object.keys(options.mcpServers||{}).map(name=>({name,status:'connected'}));
 };
 return {
  mcpServerStatus:async()=>{log({kind:'status'});return status();},
  reconnectMcpServer:async(name)=>log({kind:'reconnect',name}),
  supportedModels:async()=>[],
  accountInfo:async()=>({email:'test@example.test',subscriptionType:'Claude Max',apiProvider:'firstParty'}),
  initializationResult:async()=>({commands:[]}),
  setModel:async()=>{},setPermissionMode:async()=>{},applyFlagSettings:async()=>{},stopTask:async()=>{},
  setMcpServers:async(servers)=>{log({kind:'setMcpServers',servers});return {added:[],removed:[],errors:{}};},
  close(){abort.abort();},interrupt:async()=>abort.abort(),
  async *[Symbol.asyncIterator](){
   for await(const input of prompt){
    log({kind:'prompt'});
    yield {type:'system',subtype:'init',slash_commands:[]};
    yield {type:'user',isReplay:true,uuid:input.uuid};
    const attempts=['Read','Write','Bash','Agent','mcp__chosen__read','mcp__other__write','mcp__studio__orchestration_message','mcp__studio__orchestration_monitor'];
    const denied=[];
    for(const name of attempts){
     for(const matcher of options.hooks?.PreToolUse || []){
      for(const hook of matcher.hooks){
       const result=await hook({tool_name:name});
       if(result.hookSpecificOutput?.permissionDecision==='deny')denied.push(name);
      }
     }
    }
    log({kind:'hookDenials',denied});
    if(input.message.content.some(part=>part.text==='hold')){
     await new Promise(resolve=>abort.signal.addEventListener('abort',resolve,{once:true}));
     return;
    }
    yield {type:'result',subtype:'success',usage:{input_tokens:1,output_tokens:1},result:'done'};
   }
  }
 };
}
`;
const bridges = [];
afterEach(async () => {
  for (const bridge of bridges.splice(0)) await bridge.close();
});
async function fixture() {
  const bridge = await startBridge(FAKE_SDK, "claude-capabilities-");
  bridges.push(bridge);
  bridge.events = async () => {
    try {
      return (
        await fs.readFile(path.join(bridge.root, "sdk-events.jsonl"), "utf8")
      )
        .trim()
        .split("\n")
        .map(JSON.parse);
    } catch (error) {
      if (error.code === "ENOENT") return [];
      throw error;
    }
  };
  bridge.start = async (capabilityProfile, extra = {}) =>
    (
      await bridge.call("thread/start", {
        cwd: bridge.root,
        dynamicTools: [],
        capabilityProfile,
        ...extra,
      })
    ).thread.id;
  let sequence = 0;
  bridge.run = async (threadId, extra = {}) => {
    const { turn } = await bridge.call("turn/start", {
      threadId,
      clientUserMessageId: `input-${++sequence}`,
      input: [{ type: "text", text: "check capabilities" }],
      ...extra,
    });
    return (
      await bridge.note(
        (note) =>
          note.method === "turn/completed" && note.params.turn.id === turn.id,
      )
    ).params.turn;
  };
  bridge.config = async (name, value) => {
    const file = path.join(bridge.root, name);
    await fs.mkdir(path.dirname(file), { recursive: true });
    await fs.writeFile(file, JSON.stringify(value));
  };
  return bridge;
}

for (const method of ["thread/start", "thread/resume"]) {
  test(`${method} enforces the tool allowlist, adds denials, and restricts Studio tools`, async () => {
    const bridge = await fixture();
    const capabilityProfile = {
      allowedTools: ["Read", "Bash", "Agent", "mcp__chosen__*"],
      disallowedTools: ["Bash"],
      maxTurns: 2,
    };
    let id;
    if (method === "thread/start") id = await bridge.start(capabilityProfile);
    else {
      id = await bridge.start(null);
      assert.equal((await bridge.run(id)).status, "completed");
      await bridge.call(method, { threadId: id, capabilityProfile });
    }
    assert.equal((await bridge.run(id)).status, "completed");
    const events = (await bridge.events()).filter(
      (event) => event.session === id,
    );
    const options = events
      .filter((event) => event.kind === "options")
      .at(-1).options;
    assert.deepEqual(options.tools, ["Read"]);
    assert.deepEqual(options.disallowedTools, ["Agent", "Bash"]);
    assert.deepEqual(options.allowedTools, ["Read"]);
    assert.equal(Object.hasOwn(options, "maxTurns"), false);
    assert.ok(options.mcpServers.studio);
    assert.deepEqual(
      events.filter((event) => event.kind === "hookDenials").at(-1).denied,
      [
        "Write",
        "Bash",
        "Agent",
        "mcp__other__write",
        "mcp__studio__orchestration_message",
        "mcp__studio__orchestration_monitor",
      ],
    );
  });

  test(`${method} selects MCP scopes and preloads the discovered skill before the prompt`, async () => {
    const bridge = await fixture();
    await bridge.config("native-config/.claude.json", {
      mcpServers: {
        chosen: { command: "user-command" },
        other: { command: "excluded" },
      },
      projects: {
        [bridge.root]: { mcpServers: { chosen: { command: "local-command" } } },
      },
    });
    await bridge.config(".mcp.json", {
      mcpServers: { chosen: { command: "project-command" } },
    });
    const skill = path.join(bridge.root, ".claude/skills/review/SKILL.md");
    await fs.mkdir(path.dirname(skill), { recursive: true });
    await fs.writeFile(
      skill,
      "---\nname: review\n---\nCheck the changes carefully.",
    );
    const profile = { mcpServers: ["chosen"], skills: ["review"] };
    const id =
      method === "thread/start"
        ? await bridge.start(profile, {
            userMcp: false,
            developerInstructions: "Base instructions.",
          })
        : bridge.thread;
    if (method === "thread/resume")
      await bridge.call(method, {
        threadId: id,
        capabilityProfile: profile,
        userMcp: false,
        developerInstructions: "Base instructions.",
      });
    assert.equal((await bridge.run(id)).status, "completed");
    const events = (await bridge.events()).filter(
      (event) => event.session === id,
    );
    const options = events.find((event) => event.kind === "options").options;
    assert.equal(options.strictMcpConfig, true);
    assert.equal(options.env.ENABLE_CLAUDEAI_MCP_SERVERS, "false");
    assert.deepEqual(Object.keys(options.mcpServers).sort(), [
      "chosen",
      "studio",
    ]);
    assert.equal(options.mcpServers.chosen.command, "local-command");
    assert.match(options.systemPrompt.append, /Base instructions\./);
    assert.match(options.systemPrompt.append, /Check the changes carefully\./);
    assert.ok(
      events.findIndex((event) => event.kind === "status") <
        events.findIndex((event) => event.kind === "prompt"),
    );
    // Replacing the Studio catalog must retain selected external servers.
    assert.equal(
      (
        await bridge.run(id, {
          dynamicTools: [
            {
              name: "example",
              description: "Example",
              inputSchema: { type: "object", properties: {} },
            },
          ],
        })
      ).status,
      "completed",
    );
    const replacements = (await bridge.events()).filter(
      (event) => event.kind === "setMcpServers",
    );
    assert.equal(replacements.length, 2);
    assert.ok(
      replacements.every(
        (event) => event.servers.chosen.command === "local-command",
      ),
    );
  });
}

test("absent and null inherit defaults; empty arrays remove built-ins and user MCP servers", async () => {
  const bridge = await fixture();
  for (const userMcp of [undefined, false]) {
    for (const profile of [
      undefined,
      {
        allowedTools: null,
        disallowedTools: null,
        mcpServers: null,
        skills: null,
        maxTurns: null,
      },
    ]) {
      const id = await bridge.start(profile, { userMcp });
      assert.equal((await bridge.run(id)).status, "completed");
      const options = (await bridge.events()).find(
        (event) => event.session === id && event.kind === "options",
      ).options;
      assert.equal(options.tools, undefined);
      assert.equal(options.allowedTools, undefined);
      assert.equal(options.hooks, undefined);
      assert.equal(
        options.strictMcpConfig,
        userMcp === false ? true : undefined,
      );
      assert.deepEqual(options.disallowedTools, ["Agent"]);
    }
  }
  const id = await bridge.start({
    allowedTools: [],
    disallowedTools: [],
    mcpServers: [],
    skills: [],
  });
  assert.equal((await bridge.run(id)).status, "completed");
  const events = (await bridge.events()).filter(
    (event) => event.session === id,
  );
  const options = events.find((event) => event.kind === "options").options;
  assert.deepEqual(options.tools, []);
  assert.deepEqual(Object.keys(options.mcpServers), ["studio"]);
  assert.equal(options.strictMcpConfig, true);
  assert.deepEqual(
    events.find((event) => event.kind === "hookDenials").denied,
    [
      "Read",
      "Write",
      "Bash",
      "Agent",
      "mcp__chosen__read",
      "mcp__other__write",
      "mcp__studio__orchestration_message",
      "mcp__studio__orchestration_monitor",
    ],
  );
});

for (const method of ["thread/start", "thread/resume"]) {
  for (const scenario of [
    "unknown-skill",
    "unknown-server",
    "failed",
    "needs-auth",
    "pending",
    "missing-status",
  ]) {
    test(`${method} rejects ${scenario} before any prompt`, async () => {
      const bridge = await fixture();
      const profile =
        scenario === "unknown-skill"
          ? { skills: ["missing-skill"] }
          : { mcpServers: ["chosen"] };
      if (!["unknown-server", "unknown-skill"].includes(scenario)) {
        await bridge.config(".mcp.json", {
          mcpServers: { chosen: { command: "fake-server" } },
        });
        await bridge.config(
          "mcp-status.json",
          scenario === "missing-status"
            ? []
            : [{ name: "chosen", status: scenario }],
        );
      }
      const id =
        method === "thread/start" ? await bridge.start(profile) : bridge.thread;
      if (method === "thread/resume")
        await bridge.call(method, { threadId: id, capabilityProfile: profile });
      const turn = await bridge.run(id);
      assert.equal(turn.status, "failed");
      assert.match(
        turn.error.message,
        scenario === "unknown-skill"
          ? /capabilityProfile.skills "missing-skill"/
          : /capabilityProfile.mcpServers "chosen"/,
      );
      assert.equal(turn.error.data.turnStartOutcome, "not_applied");
      assert.equal(
        (await bridge.events()).filter(
          (event) => event.session === id && event.kind === "prompt",
        ).length,
        0,
      );
    });
  }
}

test("wildcard allow and deny rules use exact case-sensitive glob matches", async () => {
  const profile = {
    allowedTools: ["R*", "mcp__chosen__*"],
    disallowedTools: ["*write", "mcp__studio__*", "mcp__studio"],
  };
  const options = capabilityToolOptions(profile);
  assert.deepEqual(options.tools, { type: "preset", preset: "claude_code" });
  assert.deepEqual(options.disallowedTools, ["Agent"]);
  for (const name of ["Read", "mcp__chosen__read"])
    assert.equal(capabilityToolDenial(profile, name), null);
  for (const name of [
    "read",
    "Write",
    "Bash",
    "Agent",
    "mcp__chosen__write",
    "mcp__studio__anything",
  ])
    assert.ok(capabilityToolDenial(profile, name));
  const hook = options.hooks.PreToolUse[0].hooks[0];
  assert.equal(
    (await hook({ tool_name: "Bash" })).hookSpecificOutput.permissionDecision,
    "deny",
  );
  assert.deepEqual(await hook({ tool_name: "Read" }), {});
  assert.ok(
    capabilityToolDenial({ disallowedTools: ["*"] }, "mcp__studio__anything"),
  );
});

for (const profile of [
  { allowedTools: ["Read"] },
  { allowedTools: [] },
  { allowedTools: ["mcp__studio__*"], disallowedTools: ["mcp__studio__*"] },
]) {
  test(`Studio monitor obeys ${JSON.stringify(profile)} regardless of provenance`, async () => {
    const hook = capabilityToolOptions(profile).hooks.PreToolUse[0].hooks[0];
    for (const mcp_server of [
      undefined,
      { name: "studio", source: "sdk" },
      { name: "studio", source: "user" },
    ]) {
      const result = await hook({
        tool_name: "mcp__studio__orchestration_monitor",
        mcp_server,
      });
      assert.equal(result.hookSpecificOutput.permissionDecision, "deny");
    }
  });
}

test("SDK permission lists contain only exact permitted tool names", async () => {
  const options = capabilityToolOptions({
    allowedTools: [
      "Read",
      "Bash",
      "Agent",
      "mcp__chosen",
      "mcp__chosen__read",
      "mcp__studio__orchestration_message",
      "mcp__chosen__*",
    ],
    disallowedTools: ["Bash", "mcp__chosen__write"],
  });
  assert.deepEqual(options.allowedTools, [
    "Read",
    "mcp__chosen__read",
    "mcp__studio__orchestration_message",
  ]);
  assert.deepEqual(options.disallowedTools, [
    "Agent",
    "Bash",
    "mcp__chosen__write",
  ]);
  assert.deepEqual(options.tools, ["Read"]);
  const hook = options.hooks.PreToolUse[0].hooks[0];
  assert.deepEqual(
    await hook({
      tool_name: "mcp__studio__orchestration_message",
      mcp_server: { name: "studio", source: "sdk" },
    }),
    {},
  );
  assert.equal(
    (
      await hook({
        tool_name: "mcp__studio__orchestration_monitor",
        mcp_server: { name: "studio", source: "sdk" },
      })
    ).hookSpecificOutput.permissionDecision,
    "deny",
  );
});

test("resume without an override retains the persisted capability profile", async () => {
  const bridge = await fixture();
  const id = await bridge.start({ allowedTools: [], mcpServers: [] });
  assert.equal((await bridge.run(id)).status, "completed");
  await bridge.call("thread/unsubscribe", { threadId: id });
  await bridge.call("thread/resume", { threadId: id });
  assert.equal((await bridge.run(id)).status, "completed");
  const options = (await bridge.events())
    .filter((event) => event.session === id && event.kind === "options")
    .at(-1).options;
  assert.deepEqual(options.tools, []);
  assert.equal(options.strictMcpConfig, true);
});

test("a live resume rejects a changed profile and accepts the same limits", async () => {
  const bridge = await fixture();
  const capabilityProfile = { allowedTools: ["Read"], skills: [] };
  const id = await bridge.start(capabilityProfile);
  await bridge.call("turn/start", {
    threadId: id,
    clientUserMessageId: "hold-input",
    input: [{ type: "text", text: "hold" }],
  });
  await assert.rejects(
    bridge.call("thread/resume", {
      threadId: id,
      capabilityProfile: { allowedTools: [] },
    }),
    /Pause Claude before changing capabilityProfile/,
  );
  const resumed = await bridge.call("thread/resume", {
    threadId: id,
    capabilityProfile: { skills: [], allowedTools: ["Read"] },
  });
  assert.equal(resumed.reattached, true);
});
