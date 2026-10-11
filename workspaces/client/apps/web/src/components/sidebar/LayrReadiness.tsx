import { forwardRef, type ButtonHTMLAttributes } from "react";
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

/** The row shows layr only where it is active: a project that is ready for layr chats. An
 * import that runs, failed or never started shows nothing. */
export function activeLayrReadiness(
  entries: readonly LayrReadinessEntry[],
): LayrReadinessEntry[] {
  return entries.filter((entry) => entry.readiness.state === "ready");
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

/** The LAYR chip under the project name. It opens the layr menu of the project: its Mac
 * folder sync and its vault. */
const ProjectLayrReadiness = forwardRef<
  HTMLButtonElement,
  {
    summary: LayrReadinessSummary | null;
    /** A report in the tooltip needs the user, for example a Mac folder sync conflict. */
    attention?: boolean;
  } & Omit<ButtonHTMLAttributes<HTMLButtonElement>, "children">
>(function ProjectLayrReadiness({ summary, attention = false, ...props }, ref) {
  if (!summary) return null;
  return (
    <button
      {...props}
      ref={ref}
      type="button"
      className="project-layr-readiness"
      data-state={summary.state}
      data-attention={attention ? "true" : undefined}
      title={summary.title}
      aria-label={`layr: ${summary.title.split("\n").join("; ")}`}
    >
      LAYR
    </button>
  );
});

export default ProjectLayrReadiness;
