import type { BridgeHarness, BridgeNoteFor } from "./testing/bridge-harness.ts";
// Exercise thread start/resume through the real bridge with an SDK fixture.
import { afterEach, test } from "vitest";
import assert from "node:assert/strict";
import fs from "node:fs/promises";
import path from "node:path";
import { startBridge } from "./testing/bridge-harness.ts";

const FAKE_SDK = new URL("./workspace-fence.fixture.mts", import.meta.url);
const bridges: BridgeHarness[] = [];
afterEach(async () => {
  for (const bridge of bridges.splice(0)) await bridge.close();
});

test("the SDK receives each thread's host worktree fence, including resume", async () => {
  const bridge = await startBridge(FAKE_SDK, "claude-workspace-fence-");
  bridges.push(bridge);
  const tree = bridge.root;
  const fence = JSON.stringify(["/source checkout", '/sibling:quoted"path']);
  const options: import("./bridge-protocol.ts").BridgeParams<"thread/start"> = {
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
  async function run(threadId: string) {
    const turn = (
      await bridge.call("turn/start", {
        threadId,
        clientUserMessageId: `input-${++input}`,
        input: [{ type: "text", text: "check environment" }],
      })
    ).turn.id;
    const note = await bridge.note(
      (n): n is BridgeNoteFor<"turn/completed"> =>
        n.method === "turn/completed" && n.params.turn.id === turn,
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
  const bridge = await startBridge(FAKE_SDK, "claude-native-layr-", {
    env: { STUDIO_FENCE_FIXTURE_RECORD: "native" },
  });
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
  const bridge = await startBridge(FAKE_SDK, "claude-native-reviewer-", {
    env: { STUDIO_FENCE_FIXTURE_RECORD: "sandbox" },
  });
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
  const bridge = await startBridge(FAKE_SDK, "claude-reviewer-env-", {
    env: {
      STUDIO_FENCE_FIXTURE_RECORD: "environment",
      STUDIO_REVIEW_TEST_SECRET: "fixture-only-secret",
      STUDIO_CLAUDE_BIN: "fixture-cli",
      STUDIO_CLAUDE_CREDENTIAL_IDENTITY: "claude:test@example.test",
    },
  });
  bridges.push(bridge);
  const scratch = path.join(bridge.root, "scratch");
  await fs.mkdir(scratch);
  const environment: Record<string, string | undefined> = {
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
