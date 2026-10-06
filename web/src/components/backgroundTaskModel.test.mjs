import assert from "node:assert/strict";
import { it } from "vitest";
import {
  activeTask,
  backgroundTasks,
  projectTaskForRenderer,
} from "./backgroundTaskModel.ts";

it("projects generated monitor and task payloads for renderer use", () => {
  const tasks = backgroundTasks({
    runtime: {
      monitors: [
        { id: "incomplete", status: null },
        {
          id: "monitor",
          agent: "lead",
          created: 1,
          status: "completed",
        },
      ],
      tasks: [
        {
          id: "task",
          agent: "lead",
          created: 2,
          status: "running",
          kind: "command",
        },
      ],
    },
  });
  assert.equal(tasks[0].kind, "monitor");
  assert.equal(activeTask(tasks[0]), false);
  assert.equal(activeTask(tasks[1]), true);
});

it("keeps a task entity visible when its optional kind is absent", () => {
  const task = projectTaskForRenderer(
    {
      id: "task-without-kind",
      agent: "lead",
      created: 1,
      status: "running",
    },
    "task",
  );
  assert.equal(task?.kind, "tool");
  assert.equal(task?.id, "task-without-kind");
});
