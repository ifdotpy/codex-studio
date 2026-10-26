import { describe, expect, it } from "vitest";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import type { Message } from "../../../types";
import Activity, { activitySummary } from "./Activity";

it("renders an empty activity group as open details", () => {
  const markup = renderToStaticMarkup(createElement(Activity, { items: [] }));
  expect(markup).toContain('<details class="tool-group"');
  expect(markup).toContain('open=""');
  expect(markup).toContain("<summary>");
});

function toolMessage(value: unknown): Message {
  return {
    id: "tool-item",
    role: "tool",
    text: JSON.stringify(value),
  };
}

describe("activitySummary", () => {
  it("keeps malformed mixed command actions from being read-only", () => {
    const summary = activitySummary([
      toolMessage({
        type: "commandExecution",
        commandActions: [{ type: "read", path: "README.md" }, null],
      }),
    ]);

    expect(summary.label).toContain("Read 1 file");
    expect(summary.label).toContain("Ran 1 command");
  });

  it("treats a present nonzero string exit code as failed", () => {
    const summary = activitySummary([
      toolMessage({ type: "commandExecution", exitCode: "1" }),
    ]);

    expect(summary.failed).toBe(1);
  });
});
