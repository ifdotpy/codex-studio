import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { MantineProvider } from "@mantine/core";
import { expect, test } from "vitest";
import {
  StudioErrorList,
  actionLabel,
  type StudioErrorItem,
} from "./StudioErrors";

const lost: StudioErrorItem = {
  id: "monitor:watch",
  kind: "monitor_outcome_lost",
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
  at: 1_792_700_000,
};

const disconnected: StudioErrorItem = {
  ...lost,
  id: "agent:worker:turn",
  kind: "turn_disconnected",
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
        onDismiss: () => undefined,
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
  expect(markup).toContain("Dismiss");
  expect(actionLabel(lost)).toBe("Open");
  expect(actionLabel(disconnected)).toBe("Resume");
});

test("an empty list says that nothing needs the owner", () => {
  expect(render([])).toContain(
    "No agent work ended because of a Studio error.",
  );
});
