import { componentSnapshot, required } from "./componentTestFixtures";
import type { BackgroundTask, Monitor } from "../types";
import type { components } from "../generated/api";
import assert from "node:assert/strict";
import { expectTypeOf, it } from "vitest";
import {
  activeTask,
  backgroundTasks,
  displayProcess,
  processIdentity,
  projectTaskForRenderer,
  type DisplayLegacyTask,
  type DisplayProcessTask,
  type RendererTaskKind,
} from "./backgroundTaskModel.ts";

it("projects generated monitor and task payloads for renderer use", () => {
  const tasks = backgroundTasks(
    componentSnapshot([], {
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
    }),
  );
  assert.equal(required(tasks[0]).kind, "monitor");
  assert.equal(activeTask(required(tasks[0])), false);
  assert.equal(activeTask(required(tasks[1])), true);
});

it("keeps generated process identity, capabilities, and open status through every source", () => {
  const process = {
    id: "process / arbitrary:identifier",
    key: "process:key / 1",
    generation: "generation / 2",
    agent: "agent / 3",
    accountKey: "account / 4",
    epoch: 0,
    name: "arbitrary tool name",
    command: "printf 'a command with spaces'",
    created: 1,
    status: "future process status",
    source: "monitor",
    output: "arbitrary output\n",
    capabilities: {
      readOutput: false,
      sendInput: true,
      resize: false,
      stop: true,
    },
  } satisfies components["schemas"]["ManagedProcessRecord"];
  const sources = [
    ["monitor", "monitor"],
    ["codex_task", "command"],
    ["claude_task", "backgroundTask"],
  ] as const satisfies ReadonlyArray<
    readonly [DisplayProcessTask["source"], RendererTaskKind]
  >;
  for (const [source, kind] of sources) {
    const task = displayProcess({ ...process, source });
    assert.equal(task.kind, kind);
    assert.equal(task.id, process.id);
    assert.equal(task.command, process.command);
    assert.equal(task.status, process.status);
    assert.equal(task.tail, process.output);
    assert.equal(task.interactive, process.capabilities.sendInput);
    assert.equal(task.capabilities, process.capabilities);
    assert.deepEqual(processIdentity(task), {
      key: process.key,
      generation: process.generation,
      agent: process.agent,
      accountKey: process.accountKey,
      epoch: process.epoch,
    });
  }
});

it("requires generated process identity for controls and retains closed task kinds", () => {
  expectTypeOf<DisplayProcessTask["status"]>().toEqualTypeOf<string>();
  expectTypeOf<DisplayProcessTask["capabilities"]>().toEqualTypeOf<
    components["schemas"]["ProcessCapabilities"]
  >();
  expectTypeOf<
    Parameters<typeof processIdentity>[0]
  >().toEqualTypeOf<DisplayProcessTask>();
  expectTypeOf<ReturnType<typeof processIdentity>>().toEqualTypeOf<
    components["schemas"]["ProcessIdentity"]
  >();
  expectTypeOf<BackgroundTask>().not.toExtend<DisplayProcessTask>();
  expectTypeOf<Monitor>().not.toExtend<DisplayProcessTask>();
  expectTypeOf<DisplayLegacyTask>().not.toExtend<DisplayProcessTask>();
  expectTypeOf<
    Omit<DisplayProcessTask, "key">
  >().not.toExtend<DisplayProcessTask>();
  expectTypeOf<
    Omit<DisplayProcessTask, "generation">
  >().not.toExtend<DisplayProcessTask>();
  expectTypeOf<
    Omit<DisplayProcessTask, "accountKey">
  >().not.toExtend<DisplayProcessTask>();
  expectTypeOf<
    Omit<DisplayProcessTask, "epoch">
  >().not.toExtend<DisplayProcessTask>();
  expectTypeOf<string>().not.toExtend<RendererTaskKind>();
  expectTypeOf<DisplayLegacyTask["status"]>().toEqualTypeOf<
    components["schemas"]["TaskStatus"]
  >();
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

it("lists commands and monitors without tool calls", () => {
  const common: Partial<BackgroundTask> = {
    agent: "lead",
    created: 1,
    status: "running",
  };
  const tasks = backgroundTasks(
    componentSnapshot([], {
      monitors: [{ ...common, id: "watch" }],
      tasks: [
        { ...common, id: "command", kind: "command" },
        { ...common, id: "legacy-command", command: "pwd" },
        { ...common, id: "question", kind: "tool", name: "AskUserQuestion" },
        { ...common, id: "named-tool", name: "fileChange" },
        { ...common, id: "tool-with-command", kind: "tool", command: "pwd" },
      ],
    }),
  );
  assert.deepEqual(
    tasks.map((task) => task.id),
    ["watch", "command", "legacy-command"],
  );
  assert(tasks.every(activeTask));
});
