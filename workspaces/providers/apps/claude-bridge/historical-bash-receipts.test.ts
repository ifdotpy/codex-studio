import type { Session } from "./session-state.ts";
import { afterEach, expect, test } from "vitest";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { reconcileHistoricalBash } from "./historical-bash-receipts.ts";

const sessionId = "11111111-1111-4111-8111-111111111111";
const useId = "22222222-2222-4222-8222-222222222222";
const resultId = "33333333-3333-4333-8333-333333333333";
let root: string | undefined;
afterEach(async () => {
  if (root) await fs.rm(root, { recursive: true, force: true });
  root = undefined;
});

test.each([false, true])(
  "an old foreground Bash receipt completes its item (symlinked cwd: %s)",
  async (linked) => {
    root = await fs.realpath(
      await fs.mkdtemp(path.join(os.tmpdir(), "claude-bash-receipt-")),
    );
    const project = path.join(root, "project");
    await fs.mkdir(project);
    // Claude records the real path of a project opened through a symlink.
    const cwd = linked ? path.join(root, "link") : project;
    if (linked) await fs.symlink(project, cwd);
    const args = { command: "true" };
    const session: Session = {
      id: sessionId,
      cwd,
      turns: [
        {
          id: "turn",
          status: "interrupted",
          items: [
            {
              id: "tool",
              type: "commandExecution",
              tool: "Bash",
              status: "inProgress",
              command: args.command,
              arguments: args,
              cwd,
            },
          ],
        },
      ],
    };
    const row = { sessionId, cwd: project, isSidechain: false };
    const rows = [
      {
        ...row,
        type: "assistant",
        uuid: useId,
        timestamp: "2026-10-26T10:00:00.000Z",
        message: {
          content: [
            { type: "tool_use", id: "tool", name: "Bash", input: args },
          ],
        },
      },
      {
        ...row,
        type: "user",
        uuid: resultId,
        parentUuid: useId,
        sourceToolAssistantUUID: useId,
        timestamp: "2026-10-26T10:00:01.000Z",
        toolUseResult: { stdout: "ok", stderr: "", interrupted: false },
        message: {
          content: [
            {
              type: "tool_result",
              tool_use_id: "tool",
              is_error: false,
              content: "ok",
            },
          ],
        },
      },
    ];
    const configDir = path.join(root, "config");
    const file = path.join(
      configDir,
      "projects",
      project.replace(/[^a-zA-Z0-9]/g, "-"),
      sessionId + ".jsonl",
    );
    await fs.mkdir(path.dirname(file), { recursive: true });
    await fs.writeFile(
      file,
      rows.map((r) => JSON.stringify(r) + "\n").join(""),
    );
    const completed: unknown[] = [];
    const count = await reconcileHistoricalBash(session, {
      configDir,
      isIdle: () => true,
      complete: (_turn, item) => completed.push(item.status),
      persist: async () => undefined,
    });
    expect(count).toBe(1);
    expect(completed).toEqual(["completed"]);
  },
);
