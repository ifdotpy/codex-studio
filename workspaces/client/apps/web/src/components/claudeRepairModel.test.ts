import { expect, it } from "vitest";
import {
  missingRepairEvidence,
  repairReason,
  type ClaudeRepairReport,
} from "./claudeRepairModel";

const report: ClaudeRepairReport = {
  status: "needs_repair",
  skippedRows: 3,
  invalidSequence: true,
  scopeConfirmed: true,
  nativeHistoryConfirmed: false,
};
it("explains a sequence gap and the missing native evidence", () => {
  expect(repairReason(report)).toContain("sequence gap");
  expect(missingRepairEvidence(report)).toEqual([
    "A complete journal sequence.",
    "Native history that confirms the saved turns and their content.",
  ]);
});
it("does not invent proof for old reports", () => {
  expect(
    missingRepairEvidence({
      ...report,
      invalidSequence: false,
      scopeConfirmed: null,
      nativeHistoryConfirmed: null,
    }),
  ).toHaveLength(2);
});
it("distinguishes a confirmed repair", () => {
  const repaired: ClaudeRepairReport = {
    ...report,
    status: "repaired",
    invalidSequence: false,
    nativeHistoryConfirmed: true,
  };
  expect(repairReason(repaired)).toContain("repaired");
  expect(missingRepairEvidence(repaired)).toEqual([]);
});
