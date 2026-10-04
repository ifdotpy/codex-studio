import assert from "node:assert/strict";
import { it } from "vitest";
import { activeTask, backgroundTasks } from "./backgroundTaskModel.ts";

it("safely combines generated monitor and task payloads with nullable runtime", () => {
  assert.deepEqual(backgroundTasks({ runtime: null }), []);
  const tasks = backgroundTasks({
    runtime: {
      monitors: [{ id: "monitor", status: null }],
      tasks: [{ id: "task", status: "running", kind: "command" }],
    },
  });
  assert.equal(tasks[0].kind, "monitor");
  assert.equal(activeTask(tasks[0]), false);
  assert.equal(activeTask(tasks[1]), true);
});
