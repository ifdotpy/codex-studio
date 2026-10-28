import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { afterEach, expect, test } from "vitest";
import {
  APP_LOG_MAX_ENTRIES,
  appendAppLog,
  clearAppLog,
  useAppLog,
} from "./appLog";

afterEach(clearAppLog);
test("evicts the oldest entries at the bounded capacity", () => {
  for (let i = 0; i <= APP_LOG_MAX_ENTRIES; i++)
    appendAppLog({ at: i, title: String(i), reason: "event" });
  let result: ReturnType<typeof useAppLog> = [];
  function Read() {
    result = useAppLog();
    return null;
  }
  renderToStaticMarkup(createElement(Read));
  expect(result).toHaveLength(APP_LOG_MAX_ENTRIES);
  expect(result[0]?.title).toBe("1");
  expect(result.at(-1)?.level).toBe("log");
});

test("deduplicates blocked transfer errors across remounts", () => {
  const entry = {
    at: 1,
    level: "error" as const,
    title: "Account transfer blocked",
    reason: "Worker: missing",
    dedupeKey: "transfer:worker:missing",
  };
  appendAppLog(entry);
  appendAppLog(entry);
  let result: ReturnType<typeof useAppLog> = [];
  function Read() {
    result = useAppLog();
    return null;
  }
  renderToStaticMarkup(createElement(Read));
  expect(result).toHaveLength(1);
});
