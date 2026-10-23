import { expect, test } from "vitest";
import {
  DEFAULT_GATE_TIMEOUT_MS,
  MAX_GATE_TIMEOUT_MS,
  MAX_RESULT_SCHEMA_BYTES,
} from "../../generated/taskReview";
import {
  gateFields,
  parseGate,
  parseResultSchema,
  requiresGateOverride,
  taskStatus,
  type ReviewTask,
} from "./taskReviewModel";

const task: ReviewTask = {
  id: "task",
  title: "Task",
  rootId: "lead",
  created: 1,
  updated: 1,
  version: 1,
  status: "review",
  gate: { command: "check", timeout_ms: DEFAULT_GATE_TIMEOUT_MS },
};
const result = {
  id: "result",
  agent: "worker",
  text: "done",
  checks: "checked",
  revision: "abc",
  files: [],
  created: 2,
};

test("acceptance requires an override for missing, failed, running, and unknown gate evidence", () => {
  expect(requiresGateOverride({ ...task, gate: null })).toBe(false);
  expect(requiresGateOverride(task)).toBe(true);
  for (const status of ["not_run", "failed", "running", "unknown"] as const)
    expect(
      requiresGateOverride({
        ...task,
        results: [
          {
            ...result,
            gate: {
              status,
              config: task.gate!,
              revision: "abc",
              cwd: null,
              monitorId: "monitor",
              exitCode: 1,
              output: "",
              log: null,
            },
          },
        ],
      }),
    ).toBe(true);
});

test("only a successful gate for the current configuration permits normal acceptance", () => {
  const passed: ReviewTask = {
    ...task,
    results: [
      {
        ...result,
        gate: {
          status: "passed",
          config: { command: "check" },
          revision: "abc",
          cwd: null,
          monitorId: "monitor",
          exitCode: 0,
          output: "ok",
          log: null,
        },
      },
    ],
  };
  expect(requiresGateOverride(passed)).toBe(false);
  expect(requiresGateOverride({ ...passed, gate: "another check" })).toBe(true);
  expect(
    requiresGateOverride({
      ...passed,
      gate: { command: "check", timeout_ms: 1 },
    }),
  ).toBe(true);
  passed.results![0].gate!.monitorId = null;
  expect(requiresGateOverride(passed)).toBe(true);
});

test("gate input validates the Rust-owned timeout bounds and supports disable", () => {
  expect(parseGate("   ", "bad")).toBeNull();
  expect(parseGate("check", "1")).toEqual({ command: "check", timeout_ms: 1 });
  expect(parseGate("check", String(MAX_GATE_TIMEOUT_MS))).toEqual({
    command: "check",
    timeout_ms: MAX_GATE_TIMEOUT_MS,
  });
  for (const timeout of ["0", "1.5", "bad", String(MAX_GATE_TIMEOUT_MS + 1)])
    expect(() => parseGate("check", timeout)).toThrow("timeout");
  expect(gateFields("check").timeout).toBe(String(DEFAULT_GATE_TIMEOUT_MS));
});

test("the schema editor preserves boolean schemas and rejects invalid JSON, shapes and size", () => {
  expect(parseResultSchema(" ")).toBeUndefined();
  expect(parseResultSchema("false")).toBe(false);
  expect(parseResultSchema('{"type":"object"}')).toEqual({ type: "object" });
  for (const text of ["{", "[]", "null", '"string"', "3"])
    expect(() => parseResultSchema(text)).toThrow();
  expect(() =>
    parseResultSchema(
      JSON.stringify({ description: "x".repeat(MAX_RESULT_SCHEMA_BYTES) }),
    ),
  ).toThrow("bytes");
});

test("the list uses dependency blocking in preference to the persisted ready status", () => {
  expect(
    taskStatus({ ...task, status: "ready", displayStatus: "blocked" }),
  ).toBe("blocked");
});
