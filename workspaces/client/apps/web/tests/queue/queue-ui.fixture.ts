import assert from "node:assert/strict";
import { randomUUID } from "node:crypto";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  readTestState,
  spawnFixture,
  rustUiFixture,
  waitForFixturePort,
} from "../playwright.ts";
import { required, responseJson } from "../messages/helpers.ts";
import type { components } from "../../src/generated/api.ts";
import type { GetResult, PostBody, PostResult } from "../../src/apiTypes.ts";

type QueueMutation = PostBody<"/api/queue">;
type QueueChange<T = QueueMutation> = T extends unknown
  ? Omit<T, "agent" | "request_id">
  : never;

export async function queueUiFixture(env: NodeJS.ProcessEnv) {
  const root = await mkdtemp(join(tmpdir(), "studio-message-queue-ui-"));
  const models = ["gpt-6-astra", "gpt-5.6-sol", "gpt-5.6-luna"].map(
    (model) =>
      ({
        model,
        isDefault: model === "gpt-5.6-sol",
        defaultReasoningEffort: "medium",
        supportedReasoningEfforts: (model === "gpt-5.6-luna"
          ? ["low", "medium", "high", "max"]
          : ["low", "medium", "high", "ultra"]
        ).map((reasoningEffort) => ({ reasoningEffort })),
        serviceTiers: [{ id: "priority" }],
      }) satisfies components["schemas"]["ProviderModel"],
  );
  const proc = spawnFixture(process.execPath, [rustUiFixture, root], {
    stdio: ["pipe", "pipe", "pipe"],
    env: {
      ...process.env,
      CODEX_BOARD_STATE_DIR: join(root, "board"),
      EXECUTION_SETTINGS_CATALOG: JSON.stringify(models),
      ...env,
    },
  });
  let log = "";
  proc.stderr?.on("data", (data: Buffer) => {
    log += data;
  });
  const port = await waitForFixturePort(proc, { timeoutMs: 30_000 });
  const origin = `http://127.0.0.1:${port}`;
  const initial = await readTestState(origin);
  const lead = required(
    initial.threads.find((agent) => agent.name === "Release lead"),
  );
  const queue = () =>
    fetch(`${origin}/api/queue?agent=${lead.id}`).then(
      responseJson<GetResult<"/api/queue">>,
    );
  const post = async (body: QueueChange) => {
    const response = await fetch(origin + "/api/queue", {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        Origin: origin,
        "X-Canvas-Token": initial.token,
      },
      body: JSON.stringify({
        agent: lead.id,
        request_id: randomUUID(),
        ...body,
      } satisfies QueueMutation),
    });
    assert.equal(response.status, 200, await response.clone().text());
    return responseJson<PostResult<"/api/queue">>(response);
  };
  for (const item of (await queue()).items) {
    const current = await queue();
    await post({
      action: "cancel",
      message_id: item.id,
      expectedText: item.text,
      expected_revision: current.revision,
    });
  }
  const wait = async (
    test: () => boolean | Promise<boolean>,
    label: string,
  ) => {
    for (let i = 0; i < 120; i++) {
      if (await test()) return;
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
    throw Error(label + "\n" + log);
  };
  return { root, initial, lead, queue, wait, origin };
}
