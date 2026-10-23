import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { MantineProvider } from "@mantine/core";
import { expect, test } from "vitest";
import type { Agent } from "../../types";
import {
  CLAUDE_PERMISSION_MODES,
  type ClaudePermissionMode,
} from "../../generated/claudePermissions";
import {
  allowedWorkerModes,
  inheritedClaudeMode,
  WorkerPermissions,
} from "./WorkerPermissions";

const lead = (mode: ClaudePermissionMode, yolo = true): Agent => ({
  id: "lead",
  provider: "claude",
  yoloMode: yolo,
  claudeOptions: { permissionMode: mode },
});

test.each(CLAUDE_PERMISSION_MODES)(
  "inherits effective %s despite legacy yolo",
  (mode) => {
    expect(inheritedClaudeMode(lead(mode))).toBe(mode);
    expect(
      allowedWorkerModes(lead(mode), false).includes("bypassPermissions"),
    ).toBe(mode === "bypassPermissions");
    expect(allowedWorkerModes(lead(mode), true)).toEqual(["plan"]);
  },
);
test("legacy modes use provider-specific yolo defaults and invalid modes fail closed", () => {
  expect(inheritedClaudeMode({ id: "lead", provider: "claude" })).toBe(
    "default",
  );
  expect(inheritedClaudeMode({ id: "lead", provider: "codex" })).toBe(
    "bypassPermissions",
  );
  expect(
    inheritedClaudeMode({ id: "lead", provider: "claude", yoloMode: true }),
  ).toBe("bypassPermissions");
  expect(
    allowedWorkerModes(
      {
        ...lead("auto"),
        claudeOptions: { permissionMode: "unknown" },
      } as unknown as Agent,
      false,
    ),
  ).toEqual([]);
});
function render(provider: string, reviewer = false, yolo = false) {
  return renderToStaticMarkup(
    createElement(
      MantineProvider,
      null,
      createElement(WorkerPermissions, {
        lead: lead("auto", yolo),
        provider,
        reviewer,
        value: undefined,
        onChange: () => {},
      }),
    ),
  );
}
test("Claude shows inheritance and the constrained selector", () => {
  const html = render("claude");
  expect(html).toContain("Inherits: auto from lead");
  expect(html).toContain("Permission mode");
  expect(html).not.toContain('value="bypassPermissions"');
  expect(render("claude", true)).toContain("Reviewers always use plan mode.");
});
test("Codex shows the actual sandbox and approval mapping", () => {
  expect(render("codex")).toContain(
    "Codex sandbox: workspace-write. Approval policy: on-request.",
  );
  expect(render("codex", true)).toContain("Codex sandbox: read-only.");
  expect(render("codex", true, true)).toContain(
    "Codex sandbox: danger-full-access. Approval policy: never.",
  );
  expect(render("codex")).not.toContain("Permission mode");
});
