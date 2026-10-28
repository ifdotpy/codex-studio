import { StudioErrorLevel } from "../generated/api";
import { appendAppLog, clearAppLog } from "../appLog";
import { StudioErrorEffectCertainty as GeneratedApiStudioErrorEffectCertainty } from "../generated/api.ts";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { MantineProvider } from "@mantine/core";
import { expect, test } from "vitest";
import {
  StudioErrorList,
  actionLabel,
  StudioErrorsProvider,
  useStudioErrors,
  type StudioErrorItem,
} from "./StudioErrors";

const lost: StudioErrorItem = {
  id: "monitor:watch",
  kind: "monitor_outcome_lost",
  level: StudioErrorLevel.error,
  reason:
    "Studio lost the result of this command. The command may have run. Studio does not run it again.",
  action: "check",
  agentId: "worker",
  agentName: "Fixture worker",
  monitorId: "watch",
  eventId: null,
  detail:
    "Command outcome unknown after losing the provider result; not rerun.",
  subject: "cargo test",
  ruleId: null,
  count: 1,
  firstAt: 1_792_700_000,
  at: 1_792_700_000,
  diagnostics: {
    context: {
      source: "monitor",
      requestId: "original-command",
      turnId: null,
      epoch: 0,
    },
    certainty: GeneratedApiStudioErrorEffectCertainty.unknown,
    causes: [],
  },
};

const disconnected: StudioErrorItem = {
  ...lost,
  id: "agent:worker:turn",
  kind: "turn_disconnected",
  level: StudioErrorLevel.error,
  reason: "The provider connection ended during the turn.",
  action: "resume",
  monitorId: null,
  subject: "",
};

function render(items: StudioErrorItem[]) {
  return renderToStaticMarkup(
    createElement(
      MantineProvider,
      null,
      createElement(StudioErrorList, {
        items,
        busy: null,
        onAction: () => undefined,
        selected: new Set<string>(),
        onSelect: () => undefined,
        onSelectAll: () => undefined,
        onDelete: () => undefined,
      }),
    ),
  );
}

test("each row shows its reason and one safe action", () => {
  const markup = render([lost, disconnected]);
  expect(markup).toContain("Fixture worker");
  expect(markup).toContain("Studio does not run it again.");
  expect(markup).toContain("cargo test");
  expect(markup).toContain('data-kind="monitor_outcome_lost"');
  expect(markup).toContain("Delete selected");
  expect(actionLabel(lost)).toBe("Open");
  expect(actionLabel(disconnected)).toBe("Resume");
});

test("a repeated failure shows its count and time range once", () => {
  const repeated: StudioErrorItem = {
    ...lost,
    id: "monitors:rule:watch:monitor_not_recorded",
    kind: "monitor_not_recorded",
    ruleId: "watch",
    count: 74,
    firstAt: 1_792_600_000,
  };
  const markup = render([repeated]);
  expect(markup).toContain("74 times");
  expect(markup).toContain(new Date(1_792_600_000 * 1000).toLocaleString());
  expect(markup).toContain(new Date(1_792_700_000 * 1000).toLocaleString());
  expect(render([lost])).not.toContain("times");
});

test("the full diagnostic record is available in expandable details", () => {
  const markup = render([lost]);
  expect(markup).toContain("<details");
  expect(markup).toContain("<summary>Details</summary>");
  expect(markup).toContain(lost.id);
  expect(markup).toContain("<pre");
});

test("server incidents expose details without inventing an agent action", () => {
  const incident: StudioErrorItem = {
    id: "http:failed-read",
    kind: "http_failure",
    level: StudioErrorLevel.error,
    action: "details",
    reason: "The server could not complete this request.",
    detail: "store_data",
    subject: "GET /api/monitor/log",
    at: 1,
    diagnostics: {
      context: {
        source: "http",
        route: "GET /api/monitor/log",
        requestId: "exact-read",
        status: 503,
        class: "store_data",
      },
      certainty: GeneratedApiStudioErrorEffectCertainty.read_only,
      causes: [
        {
          kind: "stored_record",
          object: "monitors",
          id: "monitor",
          pointer: "/log",
        },
      ],
    },
  };
  const markup = render([incident]);
  expect(markup).toContain("Select error http:failed-read for Server");
  expect(markup).toContain("exact-read");
  expect(markup).toContain("/log");
  expect(markup).not.toContain("agentId");
  expect(markup).not.toContain(">Open<");
  expect(markup).not.toContain(">Resume<");
});

test("an empty list says that nothing needs the owner", () => {
  expect(render([])).toContain(
    "No agent work ended because of a Studio error.",
  );
});

test("rows provide named checkboxes and bulk controls", () => {
  const markup = render([lost, disconnected]);
  expect(markup).toContain('type="checkbox"');
  expect(markup).toContain(`Select error ${lost.id} for Fixture worker`);
  expect(markup).toContain("Select all");
  expect(markup).toMatch(/<button[^>]*disabled[^>]*>[^]*?Delete selected/);
});

// The warning owner must start from the stream projection, with no HTTP hydration.
test("the server-scoped warning owner consumes streamed error rows", () => {
  function Count() {
    return createElement(
      "span",
      null,
      `Streamed errors: ${useStudioErrors().items.length}`,
    );
  }
  const markup = renderToStaticMarkup(
    createElement(
      StudioErrorsProvider,
      { items: [lost, disconnected] },
      createElement(Count),
    ),
  );
  expect(markup).toContain("Streamed errors: 2");
});

test("levels are counted and displayed in severity order", () => {
  const warning: StudioErrorItem = {
    ...lost,
    id: "warning",
    level: StudioErrorLevel.warning,
    at: 9,
  };
  const critical: StudioErrorItem = {
    ...lost,
    id: "critical",
    level: StudioErrorLevel.critical,
    at: 1,
  };
  const markup = render([warning, critical, lost]);
  expect(markup).toContain("CRITICAL (1)");
  expect(markup).toContain("ERROR (1)");
  expect(markup).toContain("WARNING (1)");
  expect(markup.indexOf('data-kind="monitor_outcome_lost"')).toBeGreaterThan(
    -1,
  );
  expect(markup.indexOf("Select error critical")).toBeLessThan(
    markup.indexOf("Select error warning"),
  );
});

test("local logs share the list without a dismissal checkbox", () => {
  appendAppLog({
    at: 1234,
    title: "Reconnected",
    reason: "Connection restored",
  });
  try {
    const markup = render([]);
    expect(markup).toContain("LOG (1)");
    expect(markup).toContain("Reconnected");
    expect(markup).toContain("Clear local logs");
    expect(markup).not.toContain('type="checkbox"');
  } finally {
    clearAppLog();
  }
});

test("local transfer failures appear as errors", () => {
  appendAppLog({
    at: 1,
    level: "error",
    title: "Account transfer blocked",
    reason: "Worker: Receipt missing",
  });
  try {
    const markup = render([]);
    expect(markup).toContain("ERROR (1)");
    expect(markup).toContain("Worker: Receipt missing");
  } finally {
    clearAppLog();
  }
});
