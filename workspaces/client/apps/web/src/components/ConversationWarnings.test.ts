import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { MantineProvider } from "@mantine/core";
import { expect, test } from "vitest";
import { StudioErrorsProvider } from "./StudioErrors";
import ConversationWarnings from "./ConversationWarnings";

test("shows informational provider update notices in the warnings trigger", () => {
  const markup = renderToStaticMarkup(
    createElement(
      MantineProvider,
      null,
      createElement(
        StudioErrorsProvider,
        null,
        createElement(ConversationWarnings, {
          scope: "updates",
          accountKey: "work",
          notices: [
            {
              id: "provider-update:work",
              accountKey: "work",
              provider: "codex",
              version: "0.154.0",
              baseline: "0.153.4",
              message:
                "A newer installed Codex CLI version is selected for runtime updates, but this connection is still using 0.153.4. Available: 0.154.0.",
              at: 1,
              level: "info",
            },
          ],
          messages: [],
        }),
      ),
    ),
  );
  expect(markup).toContain('aria-label="Notices"');
  expect(markup).toContain("lucide-info");
});

test("keeps the warnings trigger reachable when there are no notices", () => {
  const markup = renderToStaticMarkup(
    createElement(
      MantineProvider,
      null,
      createElement(
        StudioErrorsProvider,
        null,
        createElement(ConversationWarnings, {
          scope: "updates",
          accountKey: "work",
          notices: [],
          messages: [],
        }),
      ),
    ),
  );
  expect(markup).toContain("conversation-warning-trigger");
});


test.each([true, false])("the footer settings icon keeps notice severity: info=%s", (info) => {
  const markup = renderToStaticMarkup(createElement(MantineProvider, null,
    createElement(StudioErrorsProvider, null, createElement(ConversationWarnings, {
      scope: "footer", accountKey: "work", utility: true, messages: [],
      notices: [{ id: "fixture", accountKey: "work", provider: "codex", message: "Fixture notice", at: 1, ...(info ? { level: "info" as const } : {}) }],
    }))));
  expect(markup).toContain("sidebar-utility-button");
  expect(markup.includes("conversation-info-trigger")).toBe(info);
});
