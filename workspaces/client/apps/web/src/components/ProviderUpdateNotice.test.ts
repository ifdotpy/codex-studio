import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { expect, test } from "vitest";
import ProviderUpdateNotice from "./ProviderUpdateNotice";

test("names installed Claude Code versions that are not in use yet", () => {
  const markup = renderToStaticMarkup(
    createElement(ProviderUpdateNotice, {
      notice: {
        provider: "claude",
        installedVersion: "2.1.278",
        availableVersion: "2.2.0",
        source: "installed",
      },
    }),
  );
  expect(markup).toContain(
    "A newer installed Claude Code version is available but not in use yet.",
  );
  expect(markup).toContain("Current: 2.1.278 · Available: 2.2.0");
});

test("renders no notice when the available version is unknown", () => {
  expect(
    renderToStaticMarkup(createElement(ProviderUpdateNotice, { notice: null })),
  ).toBe("");
});
