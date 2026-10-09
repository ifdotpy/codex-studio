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
          servers: [{ id: "local", label: "This Mac", system: "Darwin" }],
          onChoose: () => {},
          onAdd: () => {},
          onCancel: () => {},
        }),
      ),
    );
    expect(markup).toContain("Workspace");
    expect(markup).toContain("layr");
    expect(markup).toContain("ASIF");
    expect(markup).toContain("worktree");
    expect(markup).toContain("Model");
    expect(markup).toContain("Workers");
    expect(markup).toContain("Cancel");
    expect(markup).toContain("Start chat");
    expect(markup).toMatch(/data-variant="filled"[^>]*>[\s\S]*?Start chat/);
  });
});
