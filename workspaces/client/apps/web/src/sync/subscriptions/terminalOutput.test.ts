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
