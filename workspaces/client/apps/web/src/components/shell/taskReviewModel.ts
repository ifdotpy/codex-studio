import type { GetResult, PostBody } from "../../api";
import {
  DEFAULT_GATE_TIMEOUT_MS,
  MAX_GATE_TIMEOUT_MS,
  MAX_RESULT_SCHEMA_BYTES,
} from "../../generated/taskReview";

export type ReviewTask = GetResult<"/api/work">["items"][number];
export type Gate = PostBody<"/api/work">["gate"];
export const prettyJson = (value: unknown) => JSON.stringify(value, null, 2);

export function gateFields(gate: Gate) {
  return {
    command: typeof gate === "string" ? gate : gate?.command || "",
    timeout: String(
      typeof gate === "object" && gate?.timeout_ms
        ? gate.timeout_ms
        : DEFAULT_GATE_TIMEOUT_MS,
    ),
  };
}

export function parseGate(
  command: string,
  timeout: string,
): Exclude<Gate, undefined> {
  if (!command.trim()) return null;
  const milliseconds = Number(timeout);
  if (
    !Number.isInteger(milliseconds) ||
    milliseconds < 1 ||
    milliseconds > MAX_GATE_TIMEOUT_MS
  )
    throw new Error(
      `The gate timeout must be an integer from 1 to ${MAX_GATE_TIMEOUT_MS} milliseconds.`,
    );
  return { command: command.trim(), timeout_ms: milliseconds };
}

export function parseResultSchema(
  text: string,
): PostBody<"/api/work">["result_schema"] {
  if (!text.trim()) return undefined;
  if (new TextEncoder().encode(text).length > MAX_RESULT_SCHEMA_BYTES)
    throw new Error(
      `The result schema must not exceed ${MAX_RESULT_SCHEMA_BYTES} UTF-8 bytes.`,
    );
  let value: unknown;
  try {
    value = JSON.parse(text);
  } catch {
    throw new Error("The result schema must be valid JSON.");
  }
  if (
    typeof value !== "boolean" &&
    (!value || typeof value !== "object" || Array.isArray(value))
  )
    throw new Error("The result schema must be a JSON object or boolean.");
  return value as PostBody<"/api/work">["result_schema"];
}

export function requiresGateOverride(task: ReviewTask): boolean {
  if (!task.gate) return false;
  const gate = task.results?.at(-1)?.gate;
  if (
    !gate ||
    gate.status !== "passed" ||
    gate.exitCode !== 0 ||
    !gate.monitorId
  )
    return true;
  const expected = gateFields(task.gate);
  const actual = gateFields(gate.config);
  return (
    expected.command !== actual.command || expected.timeout !== actual.timeout
  );
}

export function taskStatus(task: ReviewTask) {
  return task.displayStatus || task.status;
}
