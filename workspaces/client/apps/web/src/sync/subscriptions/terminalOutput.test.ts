import { expect, it } from "vitest";
import { mergeTerminalOutput } from "./terminalOutput";
const output = (text: string, offset: number) => ({
  text,
  offset,
  status: "running" as const,
  truncated: false,
});
it("resumes Unicode output, ignores duplicates, and applies status at an unchanged cursor", () => {
  const first = output("a🙂", 2);
  const next = mergeTerminalOutput(first, output("中", 3), true);
  expect(next.text).toBe("a🙂中");
  expect(mergeTerminalOutput(next, output("中", 3), true)).toEqual(next);
  expect(
    mergeTerminalOutput(next, { ...output("", 3), status: "exited" }, true)
      .status,
  ).toBe("exited");
});
it("rejects gaps and missing local bases and replaces expired snapshots", () => {
  expect(() => mergeTerminalOutput(undefined, output("new", 9), true)).toThrow(
    "local base",
  );
  expect(() =>
    mergeTerminalOutput(output("a", 1), output("new", 9), true),
  ).toThrow("local base");
  expect(
    mergeTerminalOutput(output("a", 1), output("new", 9), false).text,
  ).toBe("new");
});

it("replaces an authoritative reset snapshot even when the terminal offset decreases", () => {
  expect(
    mergeTerminalOutput(output("old history", 40), output("new", 3), false),
  ).toEqual(output("new", 3));
  expect(
    mergeTerminalOutput(
      output("old", 3),
      { ...output("tail", 7), truncated: true },
      false,
    ).text,
  ).toBe("tail");
});

it("keeps the retained-history notice across empty status intervals", () => {
  const previous = { ...output("tail", 20), truncated: true };
  expect(
    mergeTerminalOutput(previous, { ...output("", 20), status: "exited" }, true)
      .truncated,
  ).toBe(true);
});
