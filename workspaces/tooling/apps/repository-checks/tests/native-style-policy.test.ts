import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";

// Owner: app.security in the native host's tauri.conf.json.
// Tauri adds a nonce to style-src for each inline <style> of a bundled HTML
// document unless style-src is listed in dangerousDisableAssetCspModification
// (tauri-codegen inject_nonce_token, tauri manager replace_csp_nonce). A nonce
// makes browsers ignore 'unsafe-inline' (CSP Level 3), and srcdoc previews
// inherit the parent policy. Mermaid label measurement, Mantine runtime styles
// and the static Mermaid and HTML previews then lose their styles.
// The browser witness is workspaces/client/apps/web/src/components/RichPreview.browser.spec.ts.
const configUrl = new URL(
  "../../../../client/apps/native/src-tauri/tauri.conf.json",
  import.meta.url,
);

function canModify(setting, directive) {
  if (setting === undefined || typeof setting === "boolean") return !setting;
  assert.ok(Array.isArray(setting), "unexpected CSP modification setting");
  return !setting.includes(directive);
}

test("the native host serves its configured inline style policy", async () => {
  const { security } = JSON.parse(await readFile(configUrl, "utf8")).app;
  const directives = new Map(
    security.csp
      .split(";")
      .map((part) => part.trim().split(/\s+/))
      .filter(([name]) => name)
      .map(([name, ...sources]) => [name, sources]),
  );
  const styles = directives.get("style-src") ?? [];
  assert.ok(
    styles.includes("'unsafe-inline'"),
    "style-src allows inline styles",
  );
  assert.deepEqual(
    styles.filter((source) => /^'(nonce|sha256|sha384|sha512)-/.test(source)),
    [],
    "a configured nonce or hash would disable 'unsafe-inline'",
  );
  assert.equal(
    canModify(security.dangerousDisableAssetCspModification, "style-src"),
    false,
    "Tauri must not add style nonces or hashes",
  );
  assert.equal(
    canModify(security.dangerousDisableAssetCspModification, "script-src"),
    true,
    "Tauri keeps its script nonces and hashes",
  );
});
