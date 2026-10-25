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
  sidebar: { expanded: true, onClick: action },
  messages: { count: 3, onClick: action },
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
    "Toggle conversations",
    "Messages, 3 need you",
    "Chat actions",
  ]) {
    expect(html).toContain(`aria-label="${label}"`);
  }
  expect(html.match(/<button/g)).toHaveLength(3);
  expect(html).toContain('aria-expanded="true"');
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
    sidebar: { onClick: action },
    messages: { count: 1, disabled: true, onClick: action },
  });
  expect(html.match(/<button/g)).toHaveLength(2);
  expect(html).toContain('aria-label="Messages, 1 need you"');
  expect(html).toContain('disabled=""');
  expect(html).toContain("edge-rail-badge");
});

test("keeps physical left placement and expanded sidebar state", () => {
  const html = render({
    ...props,
    geometry: { ...props.geometry!, side: "left" },
    sidebar: { expanded: true, onClick: action },
  });
  expect(html).toContain('data-side="left"');
  expect(html).toContain('aria-expanded="true"');
  expect(html).toContain("height:523px");
});

test("plain rail contains only the sidebar toggle and the existing menu", () => {
  const html = render({
    ...props,
    messages: { count: 0, onClick: action },
    sidebar: { onClick: action, expanded: true },
  });
  expect(html.match(/<button/g)).toHaveLength(2);
  expect(html).toContain('aria-label="Toggle conversations"');
  expect(html).toContain('aria-label="Chat actions"');
});
test("attention adds just the messages button with its badge", () => {
  const html = render({
    ...props,
    sidebar: { onClick: action },
    messages: { count: 3, onClick: action },
  });
  expect(html.match(/<button/g)).toHaveLength(3);
  expect(html).toContain('aria-label="Messages, 3 need you"');
  expect(html).toContain("edge-rail-badge");
});
