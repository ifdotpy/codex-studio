import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { MantineProvider } from "@mantine/core";
import { expect, test } from "vitest";
import ProviderUpdateNotices from "./ProviderUpdateNotices";

test("renders independent installed and upstream notices", () => {
  const markup = renderToStaticMarkup(
    createElement(
      MantineProvider,
      null,
      createElement(ProviderUpdateNotices, {
        notices: [
          {
            id: "provider-update:work:installed",
            accountKey: "work",
            provider: "codex",
            version: "0.154.0",
            baseline: "0.153.4",
            message:
              "An installed version is not in use yet. Installed: 0.153.4 · Available: 0.154.0.",
            at: 1,
            level: "info",
          },
          {
            id: "provider-update:work:upstream",
            accountKey: "work",
            provider: "codex",
            version: "0.155.0",
            baseline: "0.153.4",
            message:
              "A newer Codex CLI release is available upstream. Installed: 0.153.4 · Available: 0.155.0.",
            at: 1,
            level: "info",
          },
        ],
      }),
    ),
  );
  expect(markup).toContain("An installed version is not in use yet.");
  expect(markup).toContain("A newer Codex CLI release is available upstream.");
});

test("renders nothing when there is no provider update notice", () => {
  const markup = renderToStaticMarkup(
    createElement(ProviderUpdateNotices, { notices: [] }),
  );
  expect(markup).toBe("");
});
