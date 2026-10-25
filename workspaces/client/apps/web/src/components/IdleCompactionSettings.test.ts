import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { MantineProvider } from "@mantine/core";
import { expect, test } from "vitest";
import {
  IdleCompactionSettings,
  idleCompactionDefaults,
} from "./IdleCompactionSettings";
import {
  IDLE_COMPACTION_DEFAULT_MIN_TOKENS,
  IDLE_COMPACTION_MIN_TOKENS,
} from "../generated/sharedValues";

test("the Claude control defaults to off and shows all supported modes and the token bound", () => {
  expect(idleCompactionDefaults).toEqual({
    mode: "off",
    minTokens: IDLE_COMPACTION_DEFAULT_MIN_TOKENS,
  });
  const html = renderToStaticMarkup(
    createElement(
      MantineProvider,
      null,
      createElement(IdleCompactionSettings, {
        value: idleCompactionDefaults,
        disabled: false,
        onChange: () => {},
      }),
    ),
  );
  expect(html).toContain('value="off" selected');
  expect(html).toContain('value="log"');
  expect(html).toContain('value="compact"');
  expect(html).toContain(`aria-valuemin="${IDLE_COMPACTION_MIN_TOKENS}"`);
  expect(html).toContain('value="compact_on_return"');
  expect(html).toContain("five-minute");
  expect(html).toContain("30 seconds before");
});
