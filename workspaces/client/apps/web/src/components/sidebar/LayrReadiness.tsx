import type { components } from "../../generated/api";

export type LayrReadiness = components["schemas"]["ProjectLayrReadinessDto"];
/** One server's report; `server` is its alias when the sidebar combines servers. */
export type LayrReadinessEntry = { server?: string; readiness: LayrReadiness };
export type LayrReadinessSummary = {
  state: LayrReadiness["state"];
  title: string;
};

// The row shows the state that needs attention first.
const ATTENTION: LayrReadiness["state"][] = [
  "failed",
  "importing",
  "not_imported",
  "ready",
];

export function layrReadinessText(readiness: LayrReadiness): string {
  switch (readiness.state) {
    case "ready":
      return "Ready for layr chats";
    case "importing":
      return "Importing into the VM";
    case "not_imported":
      return "Not in the VM yet; the first layr chat imports it";
    case "failed":
      return readiness.error
        ? `Import failed: ${readiness.error}`
        : "Import failed";
  }
}

export function layrReadinessSummary(
  entries: readonly LayrReadinessEntry[],
): LayrReadinessSummary | null {
  if (!entries.length) return null;
  const state = ATTENTION.find((item) =>
    entries.some((entry) => entry.readiness.state === item),
  )!;
  const title = entries
    .map(
      (entry) =>
        (entry.server ? `${entry.server}: ` : "") +
        layrReadinessText(entry.readiness),
    )
    .join("\n");
  return { state, title };
}

/** The project button keeps its name; assistive technology reads the state in its description. */
export function describeWithLayrReadiness(
  summary: LayrReadinessSummary | null,
  description: string,
): string {
  if (!summary) return description;
  const lines = summary.title
    .split("\n")
    .map((line) => line.replace(/\.$/, ""));
  return `${lines.join("; ")}. ${description}`;
}

/** A quiet chip after the project name: a layr chat there starts without an import, or why not. */
export default function ProjectLayrReadiness({
  summary,
}: {
  summary: LayrReadinessSummary | null;
}) {
  if (!summary) return null;
  // Not a span: sidebar code and checks read the project name as the toggle's span.
  return (
    <small
      className="project-layr-readiness"
      data-state={summary.state}
      title={summary.title}
      aria-hidden="true"
    >
      LAYR
    </small>
  );
}
