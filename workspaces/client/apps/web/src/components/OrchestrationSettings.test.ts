import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { MantineProvider } from "@mantine/core";
import { expect, test } from "vitest";
import {
  OrchestrationNumberSetting,
  childResultHoldSpec,
  idleSessionUnloadSpec,
  type OrchestrationSettingSpec,
} from "./OrchestrationSettings";
import {
  CHILD_RESULT_COALESCE_DEFAULT_SECONDS,
  CHILD_RESULT_COALESCE_MAX_SECONDS,
  CHILD_RESULT_COALESCE_MIN_SECONDS,
  IDLE_SESSION_UNLOAD_DEFAULT_MINUTES,
  IDLE_SESSION_UNLOAD_MAX_MINUTES,
  IDLE_SESSION_UNLOAD_MIN_MINUTES,
} from "../generated/sharedValues";

function render(spec: OrchestrationSettingSpec, value: number) {
  return renderToStaticMarkup(
    createElement(
      MantineProvider,
      null,
      createElement(OrchestrationNumberSetting, {
        spec,
        value,
        disabled: false,
        onChange: () => undefined,
      }),
    ),
  );
}

test.each([
  [
    childResultHoldSpec,
    CHILD_RESULT_COALESCE_DEFAULT_SECONDS,
    CHILD_RESULT_COALESCE_MIN_SECONDS,
    CHILD_RESULT_COALESCE_MAX_SECONDS,
    "0 delivers every result immediately",
  ],
  [
    idleSessionUnloadSpec,
    IDLE_SESSION_UNLOAD_DEFAULT_MINUTES,
    IDLE_SESSION_UNLOAD_MIN_MINUTES,
    IDLE_SESSION_UNLOAD_MAX_MINUTES,
    "resumed automatically on the next message",
  ],
])(
  "renders %o with its tooltip and generated bounds",
  (spec, defaultValue, min, max, phrase) => {
    const markup = render(spec, defaultValue);
    expect(markup).toContain(`aria-label="${spec.ariaLabel}"`);
    expect(markup).toContain(
      `title="${spec.tooltip.replaceAll("'", "&#x27;")}"`,
    );
    expect(markup).toContain(`value="${defaultValue}"`);
    expect(spec.tooltip).toContain(phrase);
    expect([spec.min, spec.max]).toEqual([min, max]);
    expect(max).toBeGreaterThan(defaultValue);
  },
);
