import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { MantineProvider } from "@mantine/core";
import { test, expect } from "vitest";
import { BackgroundShellSettings } from "./BackgroundShellSettings";
import { SHELL_POLICY_DEFAULTS } from "../generated/sharedValues";

test("background shell settings retain Claude defaults and explain both controls", () => {
  expect(SHELL_POLICY_DEFAULTS).toEqual({
    quietThresholdMs: 0,
    pressureCleanup: true,
  });
  const html = renderToStaticMarkup(
    createElement(
      MantineProvider,
      null,
      createElement(BackgroundShellSettings, {
        configuration: {
          settings: {
            childResultCoalesceSeconds: 10,
            idleSessionUnloadMinutes: 15,
          },
        },
        setConfiguration: () => {},
        disabled: false,
      }),
    ),
  );
  expect(html).toContain('value="0"');
  expect(html).toContain('type="checkbox"');
  expect(html).toContain("checked");
  expect(html).toContain("30 idle minutes");
  expect(html).toContain("10000 milliseconds");
});
