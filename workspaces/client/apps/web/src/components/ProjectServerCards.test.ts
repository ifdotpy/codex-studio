import { describe, expect, it } from "vitest";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { MantineProvider } from "@mantine/core";
import { theme } from "../theme";
import ProjectServerCards from "./ProjectServerCards";

describe("new chat actions", () => {
  it("offers Cancel and a primary Start chat action", () => {
    const markup = renderToStaticMarkup(
      createElement(
        MantineProvider,
        { theme },
        createElement(ProjectServerCards, {
          project: {
            id: "logical",
            path: "logical",
            name: "Project",
            locations: [
              { serverId: "local", path: "/folder", projectId: "/folder" },
            ],
          },
          servers: [
            {
              id: "local",
              label: "This computer",
              system: "Darwin",
              hasStudioVm: true,
            },
          ],
          onChoose: () => {},
          onAdd: () => {},
          onCancel: () => {},
        }),
      ),
    );
    expect(markup).toContain("Workspace");
    expect(markup).toContain("Layr");
    expect(markup).toContain("Layr VM");
    expect(markup).toContain("WT");
    expect(markup).toContain('aria-label="Agent role" role="radiogroup"');
    expect(markup).toMatch(/<input[^>]*type="radio"[^>]*value="worker"/);
    expect(markup).toMatch(/<input[^>]*type="radio"[^>]*value="review"/);
    expect(markup).toContain('role="listbox" aria-label="Main agent model"');
    expect(markup).toContain("Cancel");
    expect(markup).toContain("Start chat");
    expect(markup).toMatch(/data-variant="filled"[^>]*>[\s\S]*?Start chat/);
  });
});
