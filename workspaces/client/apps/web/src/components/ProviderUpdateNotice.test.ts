import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { expect, test } from "vitest";
import ProviderUpdateNotice from "./ProviderUpdateNotice";

test("names installed Claude Code versions that are not in use yet", () => {
  const markup = renderToStaticMarkup(
    createElement(ProviderUpdateNotice, {
      provider: "claude",
      providerLabel: "Claude Code",
      installedVersion: "2.1.278",
      notice: {
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

test("renders no notice when no newer installed version is available", () => {
  expect(
    renderToStaticMarkup(
      createElement(ProviderUpdateNotice, {
        provider: "codex",
        providerLabel: "Codex CLI",
        installedVersion: "0.153.4",
        notice: null,
      }),
    ),
  ).toBe("");
});

test("explains that a selected Codex version is not in use yet", () => {
  const markup = renderToStaticMarkup(
    createElement(ProviderUpdateNotice, {
      provider: "codex",
      providerLabel: "Codex CLI",
      installedVersion: "0.153.4",
      notice: { availableVersion: "0.154.0", source: "installed" },
    }),
  );
  expect(markup).toContain("selected for runtime updates");
  expect(markup).toContain("Current: 0.153.4 · Available: 0.154.0");
});
