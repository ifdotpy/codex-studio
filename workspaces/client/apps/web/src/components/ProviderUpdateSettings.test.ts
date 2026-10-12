import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { MantineProvider } from "@mantine/core";
import { expect, test } from "vitest";
import ProviderUpdateSettings from "./ProviderUpdateSettings";

test("renders the provider release setting with its enabled state", () => {
  const markup = renderToStaticMarkup(
    createElement(
      MantineProvider,
      null,
      createElement(ProviderUpdateSettings, {
        checked: true,
        onChange: () => undefined,
      }),
    ),
  );
  expect(markup).toContain('aria-label="Check for provider releases"');
  expect(markup).toContain('checked=""');
  expect(markup).toContain(
    "Check npm for newer Claude Code and Codex CLI releases.",
  );
});
