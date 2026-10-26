import { readFileSync } from "node:fs";
import { expect, it } from "vitest";
import contract from "../../../../contracts/mutation-journal.json";
import { serverMutationResponseIsUncertain } from "../servers/mutationIdentity";

it("keeps browser and Rust journal policy under the shared contract owner", () => {
  expect(contract.monitorInputPath).toBe("/api/monitor/input");
  for (let status = 100; status <= 599; status++)
    expect(serverMutationResponseIsUncertain(status, {})).toBe(
      status >= 500 || status === 408 || status === 429,
    );
  for (const code of contract.uncertainCodes)
    expect(serverMutationResponseIsUncertain(400, { code })).toBe(true);
  expect(serverMutationResponseIsUncertain(400, { code: "unrelated" })).toBe(
    false,
  );
  const rust = readFileSync(
    new URL(
      "../../../../packages/client-core/src/mutation_journal.rs",
      import.meta.url,
    ),
    "utf8",
  );
  expect(rust).toContain(
    'include_str!("../../../contracts/mutation-journal.json")',
  );
  for (const file of [
    "../servers/mutationIdentity.ts",
    "./mutationJournal.ts",
  ]) {
    const source = readFileSync(new URL(file, import.meta.url), "utf8");
    expect(source).toContain(
      'from "../../../../contracts/mutation-journal.json"',
    );
    expect(source).toContain("journalContract.monitorInputPath");
  }
});
