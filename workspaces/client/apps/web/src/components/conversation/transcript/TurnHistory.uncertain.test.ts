import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { MantineProvider } from "@mantine/core";
import { expect, test } from "vitest";
import type { Agent, Message } from "../../../types";
import TurnHistory from "./TurnHistory";

for (const role of ["user", "assistant"] as const) {
  test(`an unconfirmed ${role} row keeps its reason and offers no replay`, () => {
    const markup = renderToStaticMarkup(
      createElement(
        MantineProvider,
        null,
        createElement(TurnHistory, {
          items: [
            {
              id: "item",
              role,
              text: role === "user" ? "Run the command" : "",
              turnId: "turn",
              turnStatus: "uncertain",
              turnError: { message: "The native transcript is incomplete." },
            },
          ],
          enabled: true,
          agent: {
            id: "agent",
            threadId: "thread",
            provider: "claude",
            status: "paused",
          } as Agent,
          agentId: "agent",
          storageKey: "uncertain-test",
          renderMessage: (item: Message) => createElement("p", null, item.text),
          onJump: () => {},
        }),
      ),
    );
    expect(markup).toContain('data-outcome="uncertain"');
    expect(markup).toContain("Outcome unconfirmed");
    expect(markup).toContain("The native transcript is incomplete.");
    expect(markup.match(/The native transcript is incomplete\./g)).toHaveLength(
      1,
    );
    expect(markup).not.toContain("Turn interrupted");
    expect(markup).not.toContain('data-outcome="failed"');
    expect(markup).not.toContain("<button");
  });
}
