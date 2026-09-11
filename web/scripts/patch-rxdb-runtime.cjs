const fs = require("node:fs");
const path = require("node:path");

// RxDB 17.5.0 retains every event bulk and all previous document payloads.
// These caches use only get/set. Weak keys retain identity reuse without keeping
// completed event batches alive. Recheck this workaround on any RxDB upgrade.
// Projection inserts must return conflicts for server-sequence comparison.
// Other collections retain the upstream automatic reinsert behavior.
function patchRxdb(root) {
  const version = JSON.parse(
    fs.readFileSync(path.join(root, "package.json"), "utf8"),
  ).version;
  if (version !== "17.5.0")
    throw new Error(`Review the RxDB runtime patches for version ${version}.`);
  const before = "var EVENT_BULK_CACHE = new Map();";
  const after = "var EVENT_BULK_CACHE = new WeakMap();";
  const changes = ["esm", "cjs"].flatMap((format) =>
    [
      { name: "rx-change-event.js", before, after },
      {
        name: "rx-storage-helper.js",
        before: "if (error.status === 409 && !error.writeRow.previous &&",
        after:
          'if (context !== "studio-projection-pull" && error.status === 409 && !error.writeRow.previous &&',
      },
    ].map((patch) => {
      const file = path.join(root, "dist", format, patch.name);
      const source = fs.readFileSync(file, "utf8");
      const alreadyPatched = source.split(patch.after).length - 1;
      const unpatched =
        source.replace(patch.after, "").split(patch.before).length - 1;
      if (alreadyPatched + unpatched !== 1)
        throw new Error(`Unexpected RxDB runtime source: ${file}`);
      return {
        file,
        source: alreadyPatched
          ? source
          : source.replace(patch.before, patch.after),
        changed: !alreadyPatched,
      };
    }),
  );
  for (const change of changes)
    if (change.changed) fs.writeFileSync(change.file, change.source);
  return changes.some((change) => change.changed);
}
module.exports = { patchRxdb };
if (require.main === module) {
  const root = path.dirname(require.resolve("rxdb/package.json"));
  console.log(
    patchRxdb(root)
      ? "Patched RxDB event cache and projection conflicts (ESM and CJS)."
      : "RxDB runtime patches verified (ESM and CJS).",
  );
}
