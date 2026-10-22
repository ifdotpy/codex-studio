import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import path from "node:path";

// Fixture path strings predate this guard. Exact lines are grandfathered rather
// than whole files, so adding another allocation or literal still fails.
const fixturePaths = new URL("./test-temp-path-fixtures.json", import.meta.url);

export function testSource(file, source) {
  if (!/\.(rs|py|mjs|[cm]?[jt]sx?|sh)$/.test(file)) return "";
  if (
    /(?:^|\/)check-package-start\.py$|(^|\/)(tests?|fixtures|support)(\/|\.)|(^|\/)test[_-]|[._-]test[._-]|[_-](tests?|oracle|contract)\./.test(
      file,
    )
  )
    return source;
  if (file.endsWith(".rs")) {
    const test = source.search(/#\[cfg\([^\]]*\btest\b[^\]]*\)\]/);
    if (test >= 0) return source.slice(test);
  }
  return "";
}

export function temporaryStateFindings(file, source, allowed = []) {
  const code = testSource(file, source);
  const findings = [];
  if (/(?:\.|::)\s*(keep|into_path|disable_cleanup)\s*\(/.test(code))
    findings.push(
      `${file}: test temporary state must retain its cleanup owner`,
    );
  if (file.endsWith(".rs")) {
    const resourceTypes = ["TempDir", "Child"];
    for (const match of code.matchAll(/struct\s+(\w+)\s*\{([^}]+)\}/g)) {
      if (/\b(?:TempDir|Child)\b/.test(match[2])) resourceTypes.push(match[1]);
    }
    for (const owner of resourceTypes) {
      if (
        new RegExp(
          `\\b(?:OnceLock|LazyLock|OnceCell)<[^;]*\\b${owner}\\b`,
        ).test(code)
      ) {
        findings.push(
          `${file}: static caches must hold fixture bytes, not ${owner} resources`,
        );
      }
    }
  }
  const remaining = [...allowed];
  for (const line of code.split("\n")) {
    if (!/[/]tmp\b/.test(line)) continue;
    const index = remaining.indexOf(line.trim());
    if (index >= 0) remaining.splice(index, 1);
    else
      findings.push(
        `${file}: use the platform temporary directory: ${line.trim()}`,
      );
  }
  for (const stale of remaining)
    findings.push(
      `${file}: remove retired temporary-path fixture allowance: ${stale}`,
    );
  return findings;
}

export function repositoryTemporaryStateFindings(root) {
  const allowed = JSON.parse(readFileSync(fixturePaths, "utf8"));
  const files = execFileSync("git", ["ls-files", "-z"], {
    cwd: root,
    encoding: "utf8",
  })
    .split("\0")
    .filter(Boolean);
  return files.flatMap((file) => {
    if (!/\.(rs|py|mjs|[cm]?[jt]sx?|sh)$/.test(file)) return [];
    const source = readFileSync(path.join(root, file), "utf8");
    return temporaryStateFindings(file, source, allowed[file]);
  });
}
