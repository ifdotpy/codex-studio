// Keep the host CLI alive through OAuth refresh without supplying a model prompt.
import { query } from "@anthropic-ai/claude-agent-sdk";
import { createInterface } from "node:readline";

let release;
const held = new Promise((resolve) => (release = resolve));
// oxlint-disable-next-line require-yield
async function* prompt() {
  await held;
}
const controller = new AbortController();
let timer;
const q = query({
  prompt: prompt(),
  options: {
    pathToClaudeCodeExecutable: process.env.STUDIO_CLAUDE_BIN || "claude",
    env: {
      ...process.env,
      ENABLE_CLAUDEAI_MCP_SERVERS: "false",
      CLAUDE_CODE_AUTO_CONNECT_IDE: "0",
    },
    settingSources: ["user"],
    settings: { disableAllHooks: true },
    persistSession: false,
    mcpServers: {},
    strictMcpConfig: true,
    allowedTools: [],
    abortController: controller,
  },
});
function verify(account) {
  if (
    !process.env.STUDIO_CLAUDE_ACCOUNT ||
    account?.email !== process.env.STUDIO_CLAUDE_ACCOUNT ||
    account?.apiProvider !== "firstParty" ||
    (account?.apiKeySource && account.apiKeySource !== "none")
  ) {
    throw new Error("The host Claude account identity changed");
  }
}
const lines = createInterface({ input: process.stdin });
timer = setTimeout(() => {
  process.exitCode = 1;
  controller.abort();
  lines.close();
  release();
  q.close();
}, 120000);
try {
  const initial = await q.accountInfo();
  // Initialization can precede the native refresh and profile metadata read.
  // Keep the unsubmitted query alive until its token is saved, then verify it.
  if (initial?.email) verify(initial);
  process.stdout.write(JSON.stringify({ ready: true }) + "\n");
  for await (const line of lines) {
    if (line !== "done") throw new Error("Invalid host refresh completion");
    verify((await q.reinitialize()).account);
    break;
  }
} catch {
  process.exitCode = 1;
} finally {
  clearTimeout(timer);
  release();
  lines.close();
  q.close();
}
