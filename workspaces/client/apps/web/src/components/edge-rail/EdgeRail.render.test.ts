import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { MantineProvider } from "@mantine/core";
import { expect, test } from "vitest";
import { EdgeRail, type EdgeRailProps } from "./EdgeRail";

const action = () => {};
const props: EdgeRailProps = {
  geometry: {
    side: "right",
    width: 84,
    top: 112,
    bottom: 34,
    height: 523,
    targetSize: 44,
  },
  team: { busy: 2, total: 5, expanded: true, onClick: action },
  messages: { count: 3, onClick: action },
  onChanges: action,
  onTasks: action,
  onSettings: action,
  terminal: { expanded: false, onClick: action },
  menu: "Existing action",
};

function render(value: EdgeRailProps) {
  return renderToStaticMarkup(
    createElement(MantineProvider, null, createElement(EdgeRail, value)),
  );
}

test("renders accessible icon actions with the existing counts and panel state", () => {
  const html = render(props);
  for (const label of [
    "Team, 2 busy of 5",
    "Messages, 3 need you",
    "Changes",
    "Tasks",
    "Chat settings",
    "Show terminals",
    "Chat actions",
  ]) {
    expect(html).toContain(`aria-label="${label}"`);
  }
  expect(html.match(/<button/g)).toHaveLength(7);
  expect(html).toContain('aria-expanded="true"');
  expect(html).toContain("2/5");
  expect(html).toContain('data-side="right"');
});

test("does not render duplicate or hidden controls without geometry", () => {
  expect(
    renderToStaticMarkup(createElement(EdgeRail, { ...props, geometry: null })),
  ).toBe("");
});

test("preserves disabled messages and hides unavailable actions", () => {
  const html = render({
    geometry: props.geometry,
    messages: { count: 0, disabled: true, onClick: action },
  });
  expect(html.match(/<button/g)).toHaveLength(1);
  expect(html).toContain('aria-label="Messages"');
  expect(html).toContain('disabled=""');
  expect(html).not.toContain("edge-rail-badge");
});

test("keeps physical left placement and expanded terminal state", () => {
  const html = render({
    ...props,
    geometry: { ...props.geometry!, side: "left" },
    terminal: { expanded: true, onClick: action },
  });
  expect(html).toContain('data-side="left"');
  expect(html).toContain('aria-label="Hide terminals"');
  expect(html).toContain("height:523px");
});
