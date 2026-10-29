import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import RemoteProjects, { remoteProjectGroups } from "./RemoteProjects";

const attar = {
  id: "peer:igor-mbp:/work/attar",
  path: "/work/attar",
  name: "attar",
  homeServerId: "igor-mbp",
  serverLabel: "Laptop",
};

describe("projects of paired servers in a browser client", () => {
  it("renders a remote project under the name of its server", () => {
    const html = renderToStaticMarkup(
      createElement(RemoteProjects, { groups: remoteProjectGroups([attar]) }),
    );
    expect(html).toContain('data-remote-server="igor-mbp"');
    expect(html).toContain("Laptop");
    expect(html).toContain("attar");
    expect(html).not.toContain("<button");
  });

  it("groups by server and filters with the sidebar query", () => {
    const other = {
      ...attar,
      id: "x",
      name: "docs",
      homeServerId: "b",
      serverLabel: "Desk",
    };
    expect(remoteProjectGroups([attar, other]).map(([id]) => id)).toEqual([
      "igor-mbp",
      "b",
    ]);
    expect(
      remoteProjectGroups([attar, other], "doc").map(([id]) => id),
    ).toEqual(["b"]);
    expect(remoteProjectGroups(undefined)).toEqual([]);
  });
});
