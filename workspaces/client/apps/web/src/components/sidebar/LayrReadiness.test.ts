import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { expect, test } from "vitest";
import ProjectLayrReadiness, {
  activeLayrReadiness,
  describeWithLayrReadiness,
  layrReadinessSummary,
  layrReadinessText,
  type LayrReadinessEntry,
} from "./LayrReadiness";

const render = (entries: LayrReadinessEntry[]) =>
  renderToStaticMarkup(
    createElement(ProjectLayrReadiness, {
      summary: layrReadinessSummary(entries),
    }),
  );

test.each([
  [{ state: "ready" }, "Ready for layr chats"],
  [{ state: "importing" }, "Importing into the VM"],
  [
    { state: "not_imported" },
    "Not in the VM yet; the first layr chat imports it",
  ],
  [{ state: "failed", error: "VM stopped" }, "Import failed: VM stopped"],
  [{ state: "failed", error: null }, "Import failed"],
] as const)("states %j in plain words", (readiness, text) => {
  expect(layrReadinessText(readiness)).toBe(text);
  const markup = render([{ readiness }]);
  expect(markup).toContain(`data-state="${readiness.state}"`);
  expect(markup).toContain(`title="${text}"`);
  // The project button carries the words for assistive technology.
  expect(markup).toContain('aria-hidden="true"');
  expect(markup).toMatch(
    /^<small class="project-layr-readiness"[^>]*>LAYR<\/small>$/,
  );
});

test("an attention mark sits on the chip only when asked", () => {
  const summary = layrReadinessSummary([{ readiness: { state: "ready" } }]);
  expect(
    renderToStaticMarkup(createElement(ProjectLayrReadiness, { summary })),
  ).not.toContain("data-attention");
  expect(
    renderToStaticMarkup(
      createElement(ProjectLayrReadiness, { summary, attention: true }),
    ),
  ).toContain('data-attention="true"');
});

test("shows nothing without a report", () => {
  expect(layrReadinessSummary([])).toBeNull();
  expect(render([])).toBe("");
  expect(describeWithLayrReadiness(null, "Drag to reorder.")).toBe(
    "Drag to reorder.",
  );
});

test("names each server and shows the state that needs attention first", () => {
  const summary = layrReadinessSummary([
    { server: "MAC", readiness: { state: "ready" } },
    { server: "MINI", readiness: { state: "importing" } },
  ]);
  expect(summary).toEqual({
    state: "importing",
    title: "MAC: Ready for layr chats\nMINI: Importing into the VM",
  });
  expect(describeWithLayrReadiness(summary, "Drag to reorder.")).toBe(
    "MAC: Ready for layr chats; MINI: Importing into the VM. Drag to reorder.",
  );
  const failed = layrReadinessSummary([
    { server: "MAC", readiness: { state: "not_imported" } },
    {
      server: "MINI",
      readiness: { state: "failed", error: "The VM is not running." },
    },
  ]);
  expect(failed?.state).toBe("failed");
  expect(describeWithLayrReadiness(failed, "Drag to reorder.")).toBe(
    "MAC: Not in the VM yet; the first layr chat imports it; MINI: Import failed: The VM is not running. Drag to reorder.",
  );
});

test("the row shows layr only for a ready project", () => {
  const ready = { server: "MAC", readiness: { state: "ready" as const } };
  expect(
    activeLayrReadiness([
      { readiness: { state: "not_imported" } },
      { readiness: { state: "importing" } },
      { readiness: { state: "failed", error: "VM stopped" } },
    ]),
  ).toEqual([]);
  expect(
    activeLayrReadiness([ready, { readiness: { state: "importing" } }]),
  ).toEqual([ready]);
});
