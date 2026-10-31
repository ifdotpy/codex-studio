import { pathToFileURL } from "node:url";

// Owner of the Conventional Commit subject rule. The release-please configuration
// (release-please-config.json) lists the same types, and its parity test checks it.
export const commitTypes = [
  "feat",
  "fix",
  "perf",
  "revert",
  "docs",
  "test",
  "refactor",
  "build",
  "ci",
  "chore",
  "style",
] as const;

// Optional scopes, one per workspace area.
export const commitScopes = [
  "runtime",
  "client",
  "native",
  "web",
  "desktop",
  "tooling",
  "vm",
  "release",
  "deps",
] as const;

const subjectPattern = new RegExp(
  `^(${commitTypes.join("|")})(\\((${commitScopes.join("|")})\\))?!?: \\S.*$`,
);

// Merge and revert commits made by Git or by layr keep their own subject.
const generatedPattern = /^(Merge |Revert ")/;

export function subjectError(subject: string): string | undefined {
  if (generatedPattern.test(subject) || subjectPattern.test(subject))
    return undefined;
  return (
    `Not a Conventional Commit subject: "${subject}". ` +
    `Use "type(scope): summary" or "type: summary". ` +
    `Types: ${commitTypes.join(", ")}. Scopes (optional): ${commitScopes.join(", ")}.`
  );
}

// Usage: conventional-commit.ts <subject>... ; exits 1 when any subject fails.
if (
  process.argv[1] &&
  import.meta.url === pathToFileURL(process.argv[1]).href
) {
  const errors = process.argv
    .slice(2)
    .flatMap((subject) => subjectError(subject) ?? []);
  for (const error of errors) console.error(error);
  process.exit(errors.length === 0 ? 0 : 1);
}
