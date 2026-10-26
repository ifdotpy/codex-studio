import { required } from "../componentTestFixtures";
import { readFileSync } from "node:fs";
import { expect, it } from "vitest";

it("keeps Team media decisions in parity with the CSS geometry owners", () => {
  const layout = readFileSync(
    new URL("../../design/layout.css", import.meta.url),
    "utf8",
  );
  const workspace = readFileSync(
    new URL("../../workspace-layout.css", import.meta.url),
    "utf8",
  );
  const app = readFileSync(new URL("../../App.tsx", import.meta.url), "utf8");
  const rem = (source: string, token: string) => {
    const match = source.match(new RegExp(`${token}: ([\\d.]+)rem`));
    expect(match).not.toBeNull();
    return Number(required(match?.[1]));
  };
  const dock =
    rem(layout, "--studio-form-max") + rem(layout, "--studio-team-size");
  const withSidebar = dock + rem(workspace, "--studio-sidebar-width");
  expect(workspace).toContain(
    `@container workspace-frame (min-width: ${dock}rem)`,
  );
  expect(app).toContain(
    `const teamFitsWithoutSidebar = useMediaQuery("(min-width: ${dock}rem)")`,
  );
  expect(app).toContain(
    `const teamFitsWithSidebar = useMediaQuery("(min-width: ${withSidebar}rem)")`,
  );
});
