import assert from "node:assert/strict";
import { it } from "vitest";
import { MantineProvider } from "@mantine/core";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { AccountExclusionsControl } from "./AccountExclusionsControl.tsx";
import { theme } from "../../theme";

it("lists chat account exclusions as toggles, leaving new accounts enabled", () => {
  const html = renderToStaticMarkup(
    createElement(
      MantineProvider,
      { theme },
      createElement(AccountExclusionsControl, {
        lead: {
          id: "lead",
          isLead: true,
          excludedAccountKeys: ["disabled"],
        } as never,
        accounts: {
          accounts: [
            { id: "disabled", label: "Disabled", email: "a@example.test" },
            { id: "enabled", label: "Enabled", email: "b@example.test" },
          ],
        } as never,
        refresh: async () => {},
        onError: () => {},
      }),
    ),
  );
  assert.match(html, /Accounts disabled for this chat/);
  assert.match(html, /Disabled/);
  assert.match(html, /Enabled/);
  assert.match(html, /New accounts are enabled by default/);
  assert.match(html, /checked/);
});
