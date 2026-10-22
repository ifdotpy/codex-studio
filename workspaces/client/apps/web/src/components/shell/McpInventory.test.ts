import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { MantineProvider } from "@mantine/core";
import { expect, test } from "vitest";
import { McpInventory, mcpServers } from "./McpInventory";
import type { GetResult } from "../../api";

const inventory: GetResult<"/api/capabilities"> = {
  agent: "agent", at: 1, model: "model", role: "orchestrator", managed: [], observed: [], observedNative: [], nativeInventory: "", skills: [], servers: [], errors: [],
  mcp: [{ name: "files", status: "failed", error: { message: "Connection refused" }, config: { secret: "must-not-render" } }, { name: "search", status: "connected" }], serversCursor: "next",
  mcpSession: { account: "account", thread: "thread", epoch: 1, connection: "connection", generation: 1 },
};
function render(query = "", session = inventory.mcpSession) {
  return renderToStaticMarkup(createElement(MantineProvider, null, createElement(McpInventory, { agent: "agent", initial: { ...inventory, mcpSession: session }, query, loadPage: async () => inventory, reconnect: async () => ({ status: "completed", message: "Reconnect completed." }) })));
}
test("server rows expose names, status, errors, reconnect and pagination without raw configuration", () => {
  const html = render();
  for (const text of ["files", "search", "failed", "connected", "Connection refused", "Reconnect", "Load more servers"]) expect(html).toContain(text);
  expect(html).not.toContain("must-not-render");
  expect(html).not.toContain("<pre");
});
test("server filters and missing session are explicit", () => {
  expect(render("refused")).toContain("files");
  expect(render("refused")).not.toContain("MCP server search");
  expect(render("missing")).toContain("No matching MCP servers");
  expect(render("", null)).toContain("No active session");
  expect(render("", null)).toContain("disabled");
});
test("malformed entries do not invent actionable server identities", () => {
  expect(mcpServers([null, "server", {}, { name: "" }, { serverName: "alias" }])).toEqual([{ name: "alias", status: "Unknown", error: "" }]);
});
