import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { MantineProvider } from "@mantine/core";
import { expect, test } from "vitest";
import {
  ChildResultHoldSetting,
  childResultHoldTooltip,
} from "./OrchestrationSettings";
import {
  CHILD_RESULT_COALESCE_DEFAULT_SECONDS,
  CHILD_RESULT_COALESCE_MAX_SECONDS,
} from "../generated/sharedValues";

test("renders the subagent result hold with its tooltip and generated bounds", () => {
  const markup = renderToStaticMarkup(
    createElement(
      MantineProvider,
      null,
      createElement(ChildResultHoldSetting, {
        seconds: CHILD_RESULT_COALESCE_DEFAULT_SECONDS,
        disabled: false,
        onChange: () => undefined,
      }),
    ),
  );
  expect(markup).toContain('aria-label="Subagent result hold in seconds"');
  expect(markup).toContain(
    `title="${childResultHoldTooltip.replaceAll("'", "&#x27;")}"`,
  );
  expect(markup).toContain(`value="${CHILD_RESULT_COALESCE_DEFAULT_SECONDS}"`);
  expect(childResultHoldTooltip).toContain(
    "0 delivers every result immediately",
  );
  expect(CHILD_RESULT_COALESCE_MAX_SECONDS).toBeGreaterThan(
    CHILD_RESULT_COALESCE_DEFAULT_SECONDS,
  );
});
