import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { MantineProvider } from "@mantine/core";
import { expect, test } from "vitest";
import { ProfileCapabilities } from "./ProfileCapabilities";
import { capabilityDraft, parseCapabilities } from "./profileCapabilityFields";

function render(source: Parameters<typeof capabilityDraft>[0]) {
  const value = capabilityDraft(source);
  return renderToStaticMarkup(
    createElement(
      MantineProvider,
      null,
      createElement(ProfileCapabilities, {
        value,
        errors: parseCapabilities(value).errors,
        onChange: () => {},
      }),
    ),
  );
}

test("renders five fields, inherit states, and the Codex restriction", () => {
  const markup = render({});
  for (const label of [
    "Allowed tools",
    "Disallowed tools",
    "MCP servers",
    "Skills",
    "Maximum turns",
  ]) {
    expect(markup).toContain(label);
  }
  expect(markup.match(/value="inherit" selected=""/g)).toHaveLength(4);
  expect(markup).toContain(
    "Codex does not support tool restrictions or skill preload.",
  );
  expect(markup).toContain("Leave blank to inherit the default.");
  expect(markup).not.toContain("<textarea");
});

test("explicit empty lists show their effects and editable text areas", () => {
  const markup = render({
    allowedTools: [],
    disallowedTools: [],
    mcpServers: [],
    skills: [],
    maxTurns: 4294967295,
  });
  expect(markup.match(/value="explicit" selected=""/g)).toHaveLength(4);
  expect(markup.match(/<textarea/g)).toHaveLength(4);
  for (const effect of [
    "No allowed tools.",
    "No additional denied tools.",
    "No user MCP servers.",
    "No skill preload.",
  ]) {
    expect(markup).toContain(`Explicit empty: ${effect}`);
  }
  expect(markup).toContain('value="4294967295"');
});

test("invalid entries remain visible with field errors", () => {
  const markup = render({
    allowedTools: ["Read"],
    skills: "../outside",
    maxTurns: "0",
  });
  expect(markup).toContain("Read</textarea>");
  expect(markup).toContain("../outside</textarea>");
  expect(markup).toContain("Skills: invalid name");
  expect(markup).toContain(
    "Maximum turns must be a whole number from 1 to 4294967295.",
  );
  expect(markup).toContain('aria-invalid="true"');
});
