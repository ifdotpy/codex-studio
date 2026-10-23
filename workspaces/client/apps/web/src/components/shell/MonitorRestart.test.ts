import { expect, test } from "vitest";
import { restartPatterns } from "./MonitorRestart";

test("restart editor keeps one literal pattern per nonempty line", () => {
  expect(restartPatterns("ready for requests\n\n build complete \n")).toEqual([
    "ready for requests",
    " build complete ",
  ]);
  expect(restartPatterns("\n\n")).toEqual([]);
});

test("restart patterns preserve literal spaces", () => {
  expect(restartPatterns("  ready \n ")).toEqual(["  ready ", " "]);
});
