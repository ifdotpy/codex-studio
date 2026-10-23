import type { components } from "../generated/api";

export type ClaudeRepairReport = components["schemas"]["ClaudeRepairReport"];

export function repairReason(report: ClaudeRepairReport): string {
  if (report.status === "repaired")
    return "The bridge repaired the journal after native history confirmed the saved state.";
  if (report.invalidSequence)
    return "The journal has a sequence gap or an invalid final record. The saved state cannot be confirmed.";
  if (report.scopeConfirmed === false)
    return "The saved session or workspace does not match. The journal cannot be confirmed.";
  return "The journal has malformed rows. Native history cannot confirm the saved state.";
}

export function missingRepairEvidence(report: ClaudeRepairReport): string[] {
  if (report.status === "none") return [];
  const missing: string[] = [];
  if (report.invalidSequence) missing.push("A complete journal sequence.");
  if (report.scopeConfirmed !== true)
    missing.push("Confirmation that the saved session and workspace match.");
  if (report.nativeHistoryConfirmed !== true)
    missing.push(
      "Native history that confirms the saved turns and their content.",
    );
  return missing;
}
