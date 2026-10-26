import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

const root = new URL("../../../../../", import.meta.url);
const read = (path) => readFileSync(new URL(path, root), "utf8");

test("the retired server replacement action has no route, request, or UI caller", () => {
  const spec = JSON.parse(
    read("workspaces/runtime/packages/runtime-http/openapi.json"),
  );
  const request =
    spec.paths["/api/multi-server"].post.requestBody.content["application/json"]
      .schema;
  assert.ok(!("replace_server" in request.discriminator.mapping));
  assert.ok(!("ReplaceServer" in spec.components.schemas));
  for (const path of [
    "workspaces/runtime/packages/runtime-http/src/routes/multi_server.rs",
    "workspaces/client/apps/web/src/servers/ServerManager.tsx",
    "workspaces/client/apps/web/src/servers/useServerDiscovery.ts",
    "workspaces/client/apps/web/src/servers/accessContract.ts",
    "workspaces/client/apps/web/src/servers/sidebarServices.ts",
    "workspaces/runtime/packages/runtime-services/src/multi_server.rs",
    "docs/multi-server.md",
    "docs/cross-server-orchestration.md",
  ]) {
    assert.doesNotMatch(
      read(path),
      /replace_server|replaceServer|Replace \{/,
      path,
    );
  }
  // Historical records remain readable, while management no longer offers the action.
  assert.ok(
    spec.components.schemas.AccessAudit.properties.action.enum.includes(
      "replace_server",
    ),
  );
  assert.ok(spec.components.schemas.AccessSnapshot.properties.identityAlerts);
});
